//! Cloudflare Artifacts integration.
//!
//! Artifacts is Cloudflare's Git-compatible versioned storage (repos on
//! Durable Objects, addressed by `<namespace>/<repo>`). ripgit uses it as a
//! *source of truth* for git data: clients push and clone against
//! `artifacts.cloudflare.net` directly, and ripgit mirrors the objects into
//! its own DO SQLite so the existing UI — file browser, diffs, FTS5 search —
//! works unchanged over an Artifacts-hosted repo.
//!
//! Two halves live here:
//!
//! 1. **Control plane** — the `[[artifacts]]` Worker binding. workers-rs 0.7
//!    has no typed wrapper for it, so the binding object is reached through
//!    `js_sys::Reflect` and its methods invoked as plain JS functions.
//! 2. **Data plane** — the *client* side of git smart HTTP (`upload-pack`).
//!    ripgit already speaks the server side in `git.rs`; fetching from a
//!    remote needs the mirror image of it.

use crate::git;
use crate::store;
use worker::{
    ArtifactsImportParams, ArtifactsImportSource, ArtifactsImportTarget, ArtifactsListOptions,
};
use worker::*;

/// Config key holding the upstream Artifacts git remote for this repo.
pub const CFG_REMOTE: &str = "artifacts_remote";
/// Config key holding the repo token (`art_v1_...`) used to authenticate.
pub const CFG_TOKEN: &str = "artifacts_token";
/// Config key holding the ISO timestamp of the last successful sync.
pub const CFG_LAST_SYNC: &str = "artifacts_last_sync";
/// Config key holding the repo name within the bound Artifacts namespace.
pub const CFG_REPO: &str = "artifacts_repo";
/// Config key overriding which `[[artifacts]]` binding to use.
pub const CFG_BINDING: &str = "artifacts_binding";

/// Default binding name, matching wrangler.toml.
pub const DEFAULT_BINDING: &str = "ARTIFACTS";

/// Lifetime of a minted sync token. Long enough for a large clone, short
/// enough that the credential is worthless by the time it could leak.
const TOKEN_TTL_SECS: u32 = 900;

/// A repo as returned by the Artifacts binding / REST API.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `name` is echoed back to callers of the link endpoint
pub struct ArtifactsRepo {
    pub name: String,
    pub remote: String,
    /// Short-lived credential of the form `art_v1_...?expires=<unix_ts>`.
    pub token: String,
}

// ---------------------------------------------------------------------------
// Control plane — the [[artifacts]] binding
// ---------------------------------------------------------------------------

/// Provision a repo in the bound namespace.
pub async fn create_repo(env: &Env, binding: &str, repo: &str) -> Result<ArtifactsRepo> {
    let created = env.artifacts(binding)?.create(repo).await?;
    Ok(ArtifactsRepo {
        name: created.name,
        remote: created.remote,
        token: created.token,
    })
}

/// Import an external git repo (GitHub, GitLab, self-hosted) into the
/// namespace, then mirror it here. The import runs server-side inside
/// Artifacts — ripgit never proxies the upstream clone.
pub async fn import_repo(
    env: &Env,
    binding: &str,
    source_url: &str,
    repo: &str,
    branch: Option<&str>,
) -> Result<ArtifactsRepo> {
    let mut source = ArtifactsImportSource::new(source_url);
    if let Some(b) = branch {
        source = source.branch(b);
    }
    let params = ArtifactsImportParams {
        source,
        target: ArtifactsImportTarget::new(repo),
    };
    let imported = env.artifacts(binding)?.import(&params).await?;
    Ok(ArtifactsRepo {
        name: imported.name,
        remote: imported.remote,
        token: imported.token,
    })
}

/// Resolve the remote URL and a usable credential for this repo.
///
/// Two linking modes are supported:
///
/// * **Bound** — the repo lives in this Worker's Artifacts namespace. A
///   read-scoped token is minted per sync and never persisted, so a leaked
///   config row cannot be replayed and a rotated repo needs no reconfiguring.
/// * **External** — an explicit remote URL and token were supplied for a repo
///   outside this namespace. The token is stored, because there is no binding
///   through which to mint one.
pub async fn resolve_source(env: &Env, sql: &SqlStorage) -> Result<(String, RemoteAuth)> {
    if let Some(repo) = store::get_config(sql, CFG_REPO)? {
        let binding_name =
            store::get_config(sql, CFG_BINDING)?.unwrap_or_else(|| DEFAULT_BINDING.to_string());

        // The repo's own URL cannot be read off the handle: a stub carries no
        // data properties, so every accessor on it yields an RPC proxy rather
        // than a string. list() returns the same metadata as plain JSON, so the
        // URL is looked up once there and cached.
        let remote = match store::get_config(sql, CFG_REMOTE)? {
            Some(remote) => remote,
            None => {
                let remote = lookup_remote(env, &binding_name, &repo).await?;
                store::set_config(sql, CFG_REMOTE, &remote)?;
                remote
            }
        };

        let token = mint_read_token(env, &binding_name, &repo).await?;
        return Ok((remote, RemoteAuth::artifacts(&token)));
    }

    match (
        store::get_config(sql, CFG_REMOTE)?,
        store::get_config(sql, CFG_TOKEN)?,
    ) {
        (Some(remote), Some(token)) => Ok((remote, RemoteAuth::artifacts(&token))),
        _ => Err(Error::RustError(
            "no Artifacts repo linked — link one by name (bound namespace) or by remote + token"
                .to_string(),
        )),
    }
}

/// Find a repo's git URL from `list()`, which returns plain JSON.
async fn lookup_remote(env: &Env, binding: &str, repo: &str) -> Result<String> {
    let artifacts = env.artifacts(binding)?;
    let mut cursor: Option<String> = None;

    loop {
        let mut options = ArtifactsListOptions::new().limit(100);
        if let Some(c) = &cursor {
            options = options.cursor(c.clone());
        }
        let page = artifacts.list_with_options(&options).await?;

        if let Some(found) = page
            .repos
            .iter()
            .find(|r| r.name == repo)
            .and_then(|r| r.remote.clone())
        {
            return Ok(found);
        }

        match page.cursor {
            Some(next) if !page.repos.is_empty() => cursor = Some(next),
            _ => break,
        }
    }

    Err(Error::RustError(format!(
        "artifacts repo `{}` not found in namespace",
        repo
    )))
}

/// Mint a short-lived read token for a repo, via the JS shim.
///
/// The typed binding cannot do this: `get()` resolves to a stub, and awaiting a
/// stub-valued promise never settles in Rust even though the identical call
/// resolves in JS. The shim performs the call in JS and returns plain data.
async fn mint_read_token(env: &Env, binding: &str, repo: &str) -> Result<String> {
    use worker::js_sys::Reflect;
    use worker::wasm_bindgen::JsValue;
    use worker::wasm_bindgen_futures::JsFuture;

    let raw = Reflect::get(env.as_ref(), &JsValue::from_str(binding))
        .map_err(|_| Error::RustError(format!("no `{}` binding on env", binding)))?;

    let wrapped = JsFuture::from(rpc_shim::rpc_get(&raw, repo))
        .await
        .map_err(|e| Error::RustError(format!("artifacts get failed: {:?}", e)))?;
    let handle = Reflect::get(&wrapped, &JsValue::from_str("handle"))
        .map_err(|_| Error::RustError("artifacts get returned no handle".to_string()))?;

    let token = JsFuture::from(rpc_shim::rpc_create_token(&handle, "read", TOKEN_TTL_SECS))
        .await
        .map_err(|e| Error::RustError(format!("artifacts createToken failed: {:?}", e)))?;

    Reflect::get(&token, &JsValue::from_str("plaintext"))
        .ok()
        .and_then(|v| v.as_string())
        .ok_or_else(|| Error::RustError("artifacts token had no plaintext".to_string()))
}

// ---------------------------------------------------------------------------
// Data plane — git smart HTTP client
// ---------------------------------------------------------------------------

/// Strip the `?expires=` suffix; git wants the bare secret in the header.
fn token_secret(token: &str) -> &str {
    token.split('?').next().unwrap_or(token)
}

/// How to authenticate to a git remote.
///
/// Artifacts accepts a bearer token. GitHub's git endpoints do not — they want
/// HTTP Basic with the token as the password — so the scheme travels with the
/// credential rather than being assumed by the transport.
#[derive(Debug, Clone)]
pub enum RemoteAuth {
    Bearer(String),
    Basic { user: String, secret: String },
}

impl RemoteAuth {
    /// Bearer credential for an Artifacts repo token, minus any expiry suffix.
    pub fn artifacts(token: &str) -> Self {
        RemoteAuth::Bearer(token_secret(token).to_string())
    }

    /// Basic credential for GitHub. The username is ignored by GitHub as long
    /// as the token is the password; `x-access-token` is its documented form.
    pub fn github(token: &str) -> Self {
        RemoteAuth::Basic {
            user: "x-access-token".to_string(),
            secret: token.to_string(),
        }
    }

    fn header_value(&self) -> String {
        match self {
            RemoteAuth::Bearer(token) => format!("Bearer {}", token),
            RemoteAuth::Basic { user, secret } => {
                format!("Basic {}", base64_encode(format!("{}:{}", user, secret).as_bytes()))
            }
        }
    }
}

/// Standard base64, for HTTP Basic credentials.
///
/// Hand-rolled to keep the dependency list as-is; the input is a few dozen
/// bytes of credential, so nothing here needs to be fast.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }

    out
}

fn auth_headers(auth: &RemoteAuth) -> Result<Headers> {
    let headers = Headers::new();
    headers.set("Authorization", &auth.header_value())?;
    headers.set("User-Agent", "git/2.40.0 (ripgit)")?;
    Ok(headers)
}

async fn http(
    method: Method,
    url: &str,
    auth: &RemoteAuth,
    body: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut init = RequestInit::new();
    init.with_method(method);
    let headers = auth_headers(auth)?;
    if body.is_some() {
        headers.set("Content-Type", "application/x-git-upload-pack-request")?;
        headers.set("Accept", "application/x-git-upload-pack-result")?;
    }
    init.with_headers(headers);
    if let Some(b) = body {
        init.with_body(Some(worker::js_sys::Uint8Array::from(&b[..]).into()));
    }

    let req = Request::new_with_init(url, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    if resp.status_code() >= 400 {
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::RustError(format!(
            "artifacts: {} {} -> {}: {}",
            url,
            resp.status_code(),
            resp.status_code(),
            text.chars().take(300).collect::<String>()
        )));
    }
    resp.bytes().await
}

/// A ref advertised by the remote.
#[derive(Debug, Clone)]
pub struct RemoteRef {
    pub name: String,
    pub hash: String,
}

/// `GET {remote}/info/refs?service=git-upload-pack` — discover remote refs.
pub async fn discover_refs(remote: &str, auth: &RemoteAuth) -> Result<Vec<RemoteRef>> {
    let url = format!("{}/info/refs?service=git-upload-pack", remote.trim_end_matches('/'));
    let body = http(Method::Get, &url, auth, None).await?;
    parse_ref_advertisement(&body)
}

/// Parse a smart-HTTP ref advertisement into (name, hash) pairs.
///
/// Skips the `# service=` banner, the flush after it, capabilities trailing the
/// first ref after a NUL, and any peeled `^{}` entries.
pub(crate) fn parse_ref_advertisement(body: &[u8]) -> Result<Vec<RemoteRef>> {
    let mut refs = Vec::new();
    let mut pos = 0usize;

    while let Some((line, next)) = git::read_pkt_line(body, pos) {
        pos = next;
        let Some(line) = line else { continue }; // flush pkt
        if line.starts_with(b"# service=") {
            continue;
        }
        // Capabilities follow a NUL on the first ref line.
        let line = match line.iter().position(|&b| b == 0) {
            Some(nul) => &line[..nul],
            None => line,
        };
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches(['\n', '\r']);
        let Some((hash, name)) = text.split_once(' ') else { continue };
        if hash.len() != 40 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if name.ends_with("^{}") {
            continue; // peeled tag target
        }
        refs.push(RemoteRef { name: name.to_string(), hash: hash.to_string() });
    }

    Ok(refs)
}

/// Build an `upload-pack` request: wants, then haves, then `done`.
///
/// `multi_ack_detailed` is deliberately *not* requested — without it the remote
/// replies with a single NAK before the pack, which keeps demuxing simple.
/// `side-band-64k` is requested so progress arrives out-of-band on channel 2
/// rather than interleaved into the pack.
pub(crate) fn build_fetch_request(wants: &[String], haves: &[String]) -> Vec<u8> {
    let mut buf = Vec::new();
    for (i, want) in wants.iter().enumerate() {
        let line = if i == 0 {
            format!("want {} side-band-64k ofs-delta agent=ripgit/0.1\n", want)
        } else {
            format!("want {}\n", want)
        };
        git::pkt_line_bytes(&mut buf, line.as_bytes());
    }
    buf.extend_from_slice(b"0000"); // flush ends the want list
    for have in haves {
        git::pkt_line_bytes(&mut buf, format!("have {}\n", have).as_bytes());
    }
    git::pkt_line_bytes(&mut buf, b"done\n");
    buf
}

/// Demultiplex a side-band-64k `upload-pack` response into packfile bytes.
///
/// Channel 1 carries pack data, 2 progress (discarded), 3 fatal error.
pub(crate) fn demux_sideband(body: &[u8]) -> Result<Vec<u8>> {
    let mut pack = Vec::new();
    let mut pos = 0usize;

    while let Some((line, next)) = git::read_pkt_line(body, pos) {
        pos = next;
        let Some(line) = line else { continue };
        if line.starts_with(b"NAK") || line.starts_with(b"ACK") {
            continue;
        }
        match line.first() {
            Some(1) => pack.extend_from_slice(&line[1..]),
            Some(2) => {} // progress
            Some(3) => {
                return Err(Error::RustError(format!(
                    "artifacts: remote error: {}",
                    String::from_utf8_lossy(&line[1..]).trim()
                )))
            }
            _ => {}
        }
    }

    Ok(pack)
}

/// `POST {remote}/git-upload-pack` — fetch a pack covering `wants`, minus
/// anything reachable from `haves`. An empty `haves` requests a full clone.
pub async fn fetch_pack(
    remote: &str,
    auth: &RemoteAuth,
    wants: &[String],
    haves: &[String],
) -> Result<Vec<u8>> {
    let url = format!("{}/git-upload-pack", remote.trim_end_matches('/'));
    let body = build_fetch_request(wants, haves);
    let resp = http(Method::Post, &url, auth, Some(body)).await?;
    demux_sideband(&resp)
}

// ---------------------------------------------------------------------------
// Sync — pull an Artifacts repo into this DO's SQLite
// ---------------------------------------------------------------------------

/// Outcome of one sync, surfaced to the settings page and the JSON API.
#[derive(Debug, Default, serde::Serialize)]
pub struct SyncReport {
    /// Refs whose local hash now matches the remote.
    pub updated: Vec<String>,
    /// Refs that were already up to date.
    pub unchanged: Vec<String>,
    /// Per-ref failures — a bad ref does not abort the whole sync.
    pub errors: Vec<String>,
    /// Bytes of packfile transferred.
    pub pack_bytes: usize,
    /// Whether the FTS index was rebuilt for the default branch.
    pub reindexed: bool,
}

/// Local refs as (name, hash).
fn local_refs(sql: &SqlStorage) -> Result<Vec<(String, String)>> {
    #[derive(serde::Deserialize)]
    struct Row {
        name: String,
        commit_hash: String,
    }
    let rows: Vec<Row> = sql
        .exec("SELECT name, commit_hash FROM refs ORDER BY name", None)?
        .to_array()?;
    Ok(rows.into_iter().map(|r| (r.name, r.commit_hash)).collect())
}

/// Mirror the configured Artifacts remote into this repo.
///
/// Objects already present locally are offered as `have` lines, so an
/// established mirror transfers only new objects. The resulting pack may be
/// thin — deltas against bases held only in SQLite — which
/// `process_pack_streaming` already handles via `pack::ExternalObjects`.
pub async fn sync(sql: &SqlStorage, remote: &str, auth: &RemoteAuth) -> Result<SyncReport> {
    let mut report = SyncReport::default();

    let remote_refs = discover_refs(remote, auth).await?;
    if remote_refs.is_empty() {
        return Ok(report); // empty upstream repo — nothing to mirror
    }

    // A remote advertises HEAD alongside real refs. It is a pointer, not a ref
    // to store: writing it would put a bogus `HEAD` row in the refs table, and
    // ripgit adds its own HEAD line when it later advertises this repo, so the
    // mirror would advertise HEAD twice and confuse clients cloning it.
    let head_hash = remote_refs
        .iter()
        .find(|r| r.name == "HEAD")
        .map(|r| r.hash.clone());
    let remote_refs: Vec<RemoteRef> = remote_refs
        .into_iter()
        .filter(|r| r.name.starts_with("refs/"))
        .collect();
    if remote_refs.is_empty() {
        return Ok(report);
    }

    let local = local_refs(sql)?;
    let local_by_name: std::collections::HashMap<&str, &str> = local
        .iter()
        .map(|(n, h)| (n.as_str(), h.as_str()))
        .collect();

    // Want every remote ref whose hash we don't already have locally.
    let wants: Vec<String> = remote_refs
        .iter()
        .filter(|r| local_by_name.get(r.name.as_str()) != Some(&r.hash.as_str()))
        .map(|r| r.hash.clone())
        .collect();

    for r in &remote_refs {
        if local_by_name.get(r.name.as_str()) == Some(&r.hash.as_str()) {
            report.unchanged.push(r.name.clone());
        }
    }

    if wants.is_empty() {
        return Ok(report);
    }

    // Every local commit is a candidate cut point for the remote's negotiation.
    let haves: Vec<String> = local.iter().map(|(_, h)| h.clone()).collect();

    let pack = fetch_pack(remote, auth, &wants, &haves).await?;
    report.pack_bytes = pack.len();

    if pack.len() > 4 && &pack[..4] == b"PACK" {
        let bulk_mode = store::get_config(sql, "skip_fts")?
            .map(|v| v == "1")
            .unwrap_or(false);
        git::process_pack_streaming(sql, &pack, bulk_mode)?;
    }

    // Point local refs at the remote hashes now that the objects are stored.
    for r in &remote_refs {
        // A ref we do not have yet is a *creation*, which update_ref signals with
        // the all-zero hash rather than an empty string, the same way a git client
        // does in a receive-pack command.
        let old = local_by_name
            .get(r.name.as_str())
            .copied()
            .unwrap_or(store::ZERO_HASH);
        if old == r.hash.as_str() {
            continue;
        }
        match store::update_ref(sql, &r.name, old, &r.hash) {
            Ok(()) => report.updated.push(r.name.clone()),
            Err(e) => report.errors.push(format!("{}: {}", r.name, e)),
        }
    }

    // Adopt a default branch on first sync so the UI has something to render.
    if store::get_config(sql, "default_branch")?.is_none() {
        // Follow HEAD when the advertisement gave one: the branch it points at is
        // the upstream's own answer, rather than our guess.
        let preferred = head_hash
            .as_ref()
            .and_then(|h| {
                remote_refs
                    .iter()
                    .find(|r| &r.hash == h && r.name.starts_with("refs/heads/"))
            })
            .or_else(|| remote_refs.iter().find(|r| r.name == "refs/heads/main"))
            .or_else(|| remote_refs.iter().find(|r| r.name == "refs/heads/master"))
            .or_else(|| remote_refs.iter().find(|r| r.name.starts_with("refs/heads/")));
        if let Some(r) = preferred {
            let _ = store::set_config(sql, "default_branch", &r.name);
        }
    }

    // Rebuild search only when the default branch actually moved.
    if let Some(default_ref) = store::get_config(sql, "default_branch")? {
        if let Some(r) = remote_refs.iter().find(|r| r.name == default_ref) {
            if report.updated.iter().any(|n| n == &default_ref) {
                store::rebuild_fts_index(sql, &r.hash)?;
                report.reindexed = true;
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame `payload` as a pkt-line, for building protocol fixtures.
    fn pkt(payload: &[u8]) -> Vec<u8> {
        let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn ref_advertisement_skips_banner_capabilities_and_peeled_tags() {
        let mut body = Vec::new();
        body.extend(pkt(b"# service=git-upload-pack\n"));
        body.extend_from_slice(b"0000");
        // Capabilities trail the first ref after a NUL.
        body.extend(pkt(
            b"1111111111111111111111111111111111111111 HEAD\0multi_ack side-band-64k\n",
        ));
        body.extend(pkt(
            b"2222222222222222222222222222222222222222 refs/heads/main\n",
        ));
        body.extend(pkt(
            b"3333333333333333333333333333333333333333 refs/tags/v1\n",
        ));
        body.extend(pkt(
            b"4444444444444444444444444444444444444444 refs/tags/v1^{}\n",
        ));
        body.extend_from_slice(b"0000");

        let refs = parse_ref_advertisement(&body).unwrap();
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();

        assert_eq!(names, vec!["HEAD", "refs/heads/main", "refs/tags/v1"]);
        assert_eq!(refs[0].hash, "1111111111111111111111111111111111111111");
        assert_eq!(refs[1].hash, "2222222222222222222222222222222222222222");
    }

    #[test]
    fn ref_advertisement_of_empty_repo_yields_no_refs() {
        let mut body = Vec::new();
        body.extend(pkt(b"# service=git-upload-pack\n"));
        body.extend_from_slice(b"0000");
        body.extend_from_slice(b"0000");
        assert!(parse_ref_advertisement(&body).unwrap().is_empty());
    }

    #[test]
    fn fetch_request_puts_capabilities_on_first_want_only() {
        let wants = vec!["a".repeat(40), "b".repeat(40)];
        let haves = vec!["c".repeat(40)];
        let req = String::from_utf8(build_fetch_request(&wants, &haves)).unwrap();

        assert!(req.contains(&format!("want {} side-band-64k ofs-delta", "a".repeat(40))));
        // Second want carries no capabilities.
        assert!(req.contains(&format!("want {}\n", "b".repeat(40))));
        assert!(!req.contains(&format!("want {} side-band", "b".repeat(40))));
        // Flush separates wants from haves, and `done` terminates.
        let flush = req.find("0000").unwrap();
        assert!(flush < req.find("have ").unwrap());
        assert!(req.trim_end().ends_with("done"));
    }

    #[test]
    fn fetch_request_without_haves_requests_a_full_clone() {
        let req = String::from_utf8(build_fetch_request(&["a".repeat(40)], &[])).unwrap();
        assert!(!req.contains("have "));
        assert!(req.contains("done"));
    }

    #[test]
    fn sideband_demux_keeps_channel_one_and_drops_progress() {
        let mut body = Vec::new();
        body.extend(pkt(b"NAK\n"));
        body.extend(pkt(b"\x01PACK\x00\x00"));
        body.extend(pkt(b"\x02Counting objects: 5\n"));
        body.extend(pkt(b"\x01rest-of-pack"));
        body.extend_from_slice(b"0000");

        let pack = demux_sideband(&body).unwrap();
        assert_eq!(pack, b"PACK\x00\x00rest-of-pack");
    }

    #[test]
    fn sideband_demux_surfaces_channel_three_as_an_error() {
        let mut body = Vec::new();
        body.extend(pkt(b"\x03upload-pack: not our ref\n"));
        let err = demux_sideband(&body).unwrap_err().to_string();
        assert!(err.contains("not our ref"), "unexpected error: {}", err);
    }

    #[test]
    fn base64_encodes_with_correct_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn github_auth_uses_basic_and_artifacts_uses_bearer() {
        // GitHub's git endpoints reject bearer tokens.
        assert_eq!(
            RemoteAuth::github("ghs_secret").header_value(),
            format!("Basic {}", base64_encode(b"x-access-token:ghs_secret")),
        );
        // Artifacts tokens carry an expiry suffix that must not be sent.
        assert_eq!(
            RemoteAuth::artifacts("art_v1_abc?expires=99").header_value(),
            "Bearer art_v1_abc",
        );
    }

    #[test]
    fn token_secret_strips_the_expiry_query() {
        assert_eq!(token_secret("art_v1_abc?expires=1234"), "art_v1_abc");
        assert_eq!(token_secret("art_v1_abc"), "art_v1_abc");
    }
}


/// JS shims for RPC calls whose result is a stub rather than plain data.
///
/// Two things Rust cannot do directly to an RPC stub. It cannot invoke a method
/// through `Function.prototype.call`, which the receiver rejects outright, and
/// awaiting a promise that *resolves to* a stub never settles -- the same call
/// awaited from JS resolves fine. Both are avoided by doing the call in JS and
/// handing back a plain object with the stub as a field.
mod rpc_shim {
    use worker::js_sys::Promise;
    use worker::wasm_bindgen;
    use worker::wasm_bindgen::prelude::*;

    #[wasm_bindgen(inline_js = r#"
        export function rpc_get(binding, name) {
          return Promise.resolve(binding.get(name)).then((handle) => ({ handle }));
        }
        export function rpc_create_token(handle, scope, ttl) {
          return handle.createToken(scope, ttl);
        }
    "#)]
    extern "C" {
        pub fn rpc_get(binding: &JsValue, name: &str) -> Promise;
        pub fn rpc_create_token(handle: &JsValue, scope: &str, ttl: u32) -> Promise;
    }
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Walk the Artifacts call chain one step at a time.
///
/// A hang somewhere in binding -> get -> token -> refs is invisible from the
/// outside: the request simply never returns and the log records a cancel with
/// no exception. Running the chain in prefixes localizes which call never
/// settles, because the last step that answers is the one before the culprit.
pub async fn debug_step(
    env: &Env,
    sql: &SqlStorage,
    step: &str,
    arg: Option<&str>,
) -> Result<serde_json::Value> {
    // `probe` inspects what get() hands back without awaiting it. An RPC stub
    // and a Promise are both objects with a `then`, but only one of them ever
    // calls its callback, and awaiting the wrong one hangs with no error.
    // A Reflect-based probe lived here and is deliberately gone: RPC stubs
    // reject Function.prototype.call ("the RPC receiver does not implement
    // the method \"call\""), so a binding can only be driven through the
    // typed glue, which invokes methods directly.
    let binding_name =
        store::get_config(sql, CFG_BINDING)?.unwrap_or_else(|| DEFAULT_BINDING.to_string());
    let repo = store::get_config(sql, CFG_REPO)?
        .ok_or_else(|| Error::RustError("no artifacts repo linked".to_string()))?;

    // Does routing the stub-returning call through a JS shim avoid the hang?
    if step == "get_shim" {
        use worker::js_sys::Reflect;
        use worker::wasm_bindgen::JsValue;
        use worker::wasm_bindgen_futures::JsFuture;

        let raw = Reflect::get(env.as_ref(), &JsValue::from_str(&binding_name))
            .map_err(|_| Error::RustError("binding lookup failed".to_string()))?;

        let wrapped = JsFuture::from(rpc_shim::rpc_get(&raw, &repo))
            .await
            .map_err(|e| Error::RustError(format!("shim get rejected: {:?}", e)))?;
        let handle = Reflect::get(&wrapped, &JsValue::from_str("handle"))
            .map_err(|_| Error::RustError("no `handle` on shim result".to_string()))?;

        let token = JsFuture::from(rpc_shim::rpc_create_token(&handle, "read", 900))
            .await
            .map_err(|e| Error::RustError(format!("shim createToken rejected: {:?}", e)))?;
        let plaintext = Reflect::get(&token, &JsValue::from_str("plaintext"))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        let expires = Reflect::get(&token, &JsValue::from_str("expiresAt"))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();

        return Ok(serde_json::json!({
            "shim_get": "resolved",
            "token_len": plaintext.len(),
            "expires_at": expires,
        }));
    }

    let artifacts = env.artifacts(&binding_name)?;

    // Typed create through the binding's own glue, which invokes methods
    // directly rather than through Function.prototype.call -- the only form an
    // RPC stub accepts. Reports the error verbatim so a serde mismatch on the
    // result names itself.
    if step == "typed_create" {
        let name = arg.ok_or_else(|| Error::RustError("pass ?create=<name>".to_string()))?;
        return match artifacts.create(name).await {
            Ok(created) => Ok(serde_json::json!({
                "ok": true,
                "name": created.name,
                "remote": created.remote,
                "default_branch": created.default_branch,
                "token_expires_at": created.token_expires_at,
            })),
            Err(e) => Ok(serde_json::json!({ "ok": false, "error": e.to_string() })),
        };
    }

    if step == "binding" {
        return Ok(serde_json::json!({ "binding": binding_name, "repo": repo, "ok": true }));
    }

    let handle = artifacts.get(&repo).await?;
    let remote = handle.remote();
    if step == "get" {
        return Ok(serde_json::json!({
            "remote": remote,
            "name": handle.name(),
            "default_branch": handle.default_branch(),
        }));
    }

    let token = handle
        .create_token_with_options(ArtifactsTokenScope::Read, Some(TOKEN_TTL_SECS))
        .await?;
    if step == "token" {
        // The token itself is a credential; report only its shape.
        return Ok(serde_json::json!({
            "remote": remote,
            "token_len": token.plaintext.len(),
            "expires_at": token.expires_at,
        }));
    }

    let auth = RemoteAuth::artifacts(&token.plaintext);
    let refs = discover_refs(&remote, &auth).await?;
    Ok(serde_json::json!({
        "remote": remote,
        "refs": refs.iter().map(|r| format!("{} {}", r.hash, r.name)).collect::<Vec<_>>(),
    }))
}
