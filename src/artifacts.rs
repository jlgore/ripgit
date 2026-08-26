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
use crate::pack;
use crate::store;
use worker::{
    ArtifactsImportParams, ArtifactsImportSource, ArtifactsImportTarget, ArtifactsTokenScope,
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

        let handle = env.artifacts(&binding_name)?.get(&repo).await?;
        let remote = handle
            .info()
            .await?
            .remote
            .ok_or_else(|| Error::RustError(format!("repo `{}` has no git remote", repo)))?;
        let token = handle
            .create_token_with_options(ArtifactsTokenScope::Read, Some(TOKEN_TTL_SECS))
            .await?;
        return Ok((remote, RemoteAuth::artifacts(&token.plaintext)));
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

    // The push path rejects oversized packs before parsing; the pull path must
    // too. A Durable Object has far less memory than a large repo's pack, so
    // without this the sync dies on an allocation with no usable error -- the
    // same silent hang class as the receive-pack guard was written to avoid.
    if pack.len() > pack::MAX_PACK_BYTES {
        return Err(Error::RustError(format!(
            "upstream pack is {} MB, over the {} MB limit; this repo is too large to mirror",
            pack.len() / 1_000_000,
            pack::MAX_PACK_BYTES / 1_000_000,
        )));
    }

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
