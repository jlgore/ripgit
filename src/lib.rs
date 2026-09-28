mod api;
mod artifacts;
mod authz;
mod ci;
mod diff;
mod git;
mod issues;
mod issues_web;
mod mirror;
mod pack;
mod presentation;
mod schema;
mod store;
mod web;

use crate::authz::{Access, Actor, Role};
use crate::presentation::{NegotiatedRepresentation, Representation};
use worker::*;

/// Delta compression keyframe interval. A full keyframe is stored every N
/// versions within a blob group. Worst-case reconstruction applies N-1 deltas.
pub const KEYFRAME_INTERVAL: i64 = 50;

// ---------------------------------------------------------------------------
// Access checks. Identity comes from trusted X-Ripgit-Actor-* headers (set only
// by the auth worker) and the role from X-Ripgit-Role (set only by the Worker
// entry below); see authz.rs.
// ---------------------------------------------------------------------------

fn actor_scopes(actor: &Option<Actor>) -> Vec<String> {
    actor.as_ref().map(|a| a.scopes.clone()).unwrap_or_default()
}

/// Returns a deny Response unless the caller holds at least `needed`.
/// Anonymous callers get a 401 so git knows to retry with credentials.
fn require_role(actor: &Option<Actor>, role: Role, needed: Role) -> Option<Result<Response>> {
    if role >= needed {
        return None;
    }
    match actor {
        None => Some(unauthorized_401()),
        Some(_) => Some(Response::error(
            format!("Forbidden: requires {} access to this repository", needed.as_str()),
            403,
        )),
    }
}

/// Config key recording who may read a repo without a role on it.
const CFG_VISIBILITY: &str = "visibility";

/// Who may read a repo without an explicit role.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Visibility {
    /// Anyone, including anonymous callers.
    Public,
    /// Members of the owning organization.
    Internal,
    /// Only callers with a role on the repo.
    Private,
}

impl Visibility {
    fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Internal => "internal",
            Visibility::Private => "private",
        }
    }

    /// Parse a stored or submitted value. Unknown values fail closed.
    fn parse(s: &str) -> Visibility {
        match s {
            "public" => Visibility::Public,
            "internal" => Visibility::Internal,
            _ => Visibility::Private,
        }
    }
}

/// This repo's visibility. Absent config means public, which keeps repos that
/// predate visibility tracking readable as they were.
fn visibility(sql: &SqlStorage) -> Result<Visibility> {
    Ok(store::get_config(sql, CFG_VISIBILITY)?
        .map(|v| Visibility::parse(&v))
        .unwrap_or(Visibility::Public))
}

/// The caller's role once visibility is applied: anyone may read a public
/// repo, org members may read an internal one, and a private repo is readable
/// only with an explicit role.
fn visible_role(sql: &SqlStorage, access: Access) -> Result<Role> {
    let floor = match visibility(sql)? {
        Visibility::Public => Role::Read,
        Visibility::Internal if access.org_member => Role::Read,
        _ => Role::None,
    };
    Ok(access.role.max(floor))
}

/// 401 with WWW-Authenticate so git knows to prompt for / retry with credentials.
fn unauthorized_401() -> Result<Response> {
    let mut resp = Response::error("Unauthorized: sign in to push", 401)?;
    resp.headers_mut()
        .set("WWW-Authenticate", r#"Basic realm="ripgit""#)?;
    Ok(resp)
}

/// Build a 302 redirect using an absolute URL.
///
/// `Response::error("", 302)` + manual Location header is unreliable on some
/// Cloudflare Workers runtimes ("unrecognized JavaScript object"). Using
/// `Response::redirect()` with a proper absolute URL avoids this.
fn make_redirect(base_url: &Url, path: &str) -> Result<Response> {
    let abs = format!("{}{}", base_url.origin().ascii_serialization(), path);
    let url = Url::parse(&abs).map_err(|e| Error::RustError(e.to_string()))?;
    Response::redirect(url)
}

fn negotiate_or_response(
    req: &Request,
    supported: &[Representation],
    default: Representation,
) -> std::result::Result<NegotiatedRepresentation, Result<Response>> {
    presentation::preferred_representation(req, supported, default)
        .map_err(|err| err.into_response())
}

fn finalize_negotiated(
    response: Result<Response>,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    response.and_then(|resp| presentation::finalize_response(resp, selection))
}

// ---------------------------------------------------------------------------
// Worker entry point — route to the named Repository DO
// ---------------------------------------------------------------------------

fn artifacts_same_origin(req: &Request) -> Result<bool> {
    // Browser forms send Origin. Non-browser authenticated API callers may omit it.
    let origin = req.url()?.origin().ascii_serialization();
    Ok(req.headers().get("Origin")?.map(|value| value == origin).unwrap_or(true))
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    let path = url.path();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    if path == "/api/artifacts" {
        return artifacts::list_namespace(&req, &env).await;
    }

    // /:owner/ — user profile page (parts = ["owner", ""] with trailing slash,
    // or parts = ["owner"] without). Handled at the Worker level since there
    // is no per-owner DO; it just shows push instructions.
    let is_owner_page = (parts.len() == 1 && !parts[0].is_empty())
        || (parts.len() == 2 && !parts[0].is_empty() && parts[1].is_empty());
    if is_owner_page {
        let owner = parts[0];
        let actor = Actor::from_request(&req);
        let url = req.url()?;
        // Whoever administers the namespace sees its private repos.
        let access = authz::resolve_access(&env, actor.as_ref(), owner, None).await;
        let viewer = web::Viewer {
            name: actor.as_ref().map(|a| a.name.as_str()),
            role: access.role,
        };
        let repos = list_repos(&env, owner, access).await;
        let selection = match negotiate_or_response(
            &req,
            &[Representation::Html, Representation::Markdown],
            Representation::Html,
        ) {
            Ok(selection) => selection,
            Err(resp) => return resp,
        };
        return match selection.representation() {
            Representation::Html => finalize_negotiated(
                web::page_owner_profile(owner, viewer, &url, &repos),
                &selection,
            ),
            Representation::Markdown => finalize_negotiated(
                web::page_owner_profile_markdown(
                    owner,
                    viewer,
                    &url,
                    &repos,
                    &selection,
                ),
                &selection,
            ),
            Representation::Json => unreachable!(),
        };
    }

    // /:owner/:repo/* — dispatched to a DO instance named "{owner}/{repo}".
    if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        let do_name = format!("{}/{}", parts[0], parts[1]);
        let actor = Actor::from_request(&req);
        let access = authz::resolve_access(&env, actor.as_ref(), parts[0], Some(parts[1])).await;
        // Always overwrite: whatever arrived under these names is not ours.
        let mut req = req.clone_mut()?;
        access.apply(req.headers_mut()?)?;

        let namespace = env.durable_object("REPOSITORY")?;
        let id = namespace.id_from_name(&do_name)?;
        let stub = id.get_stub()?;
        return stub.fetch_with_request(req).await;
    }

    Response::from_json(&serde_json::json!({
        "name": "ripgit",
        "version": "0.1.0",
        "description": "Git remote backed by Cloudflare Durable Objects"
    }))
}

/// Cron entry point — sweep enrolled repos for upstream changes.
///
/// The push mirror (GitHub Actions + OIDC) keeps repos fresh in normal
/// operation. This exists for when it does not fire: a missed webhook, a failed
/// run, or an Actions outage. A mirror you only find out is stale during an
/// incident is not a mirror.
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    match mirror::start_sweep(&env).await {
        Ok(Some(id)) => console_log!("mirror sweep started: {}", id),
        Ok(None) => console_log!("mirror sweep skipped: no repos enrolled"),
        Err(e) => console_error!("mirror sweep failed to start: {}", e),
    }
}

// ---------------------------------------------------------------------------
// Repository Durable Object
// ---------------------------------------------------------------------------

#[durable_object]
pub struct Repository {
    state: State,
    sql: SqlStorage,
    #[allow(dead_code)]
    env: Env,
}

impl DurableObject for Repository {
    fn new(state: State, env: Env) -> Self {
        let state = state;
        let sql = state.storage().sql();
        schema::init(&sql);
        Self { sql, env, state }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let url = req.url()?;
        let path = url.path();
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();

        // Minimum: [":owner", ":repo"]
        if parts.len() < 2 {
            return Response::error("Not Found", 404);
        }

        let owner = parts[0];
        let repo_name = parts[1];
        let action = if parts.len() >= 3 { parts[2] } else { "" };

        // Resolve the caller's identity from trusted headers (set by auth worker).
        // None means anonymous — allowed for reads, denied for writes.
        let actor = Actor::from_request(&req);
        let actor_name = actor.as_ref().map(|a| a.name.as_str());

        // Gate reads before dispatch, so every route -- pages, API, and the git
        // protocol alike -- is covered by one check rather than each remembering.
        // A private repo answers as though it does not exist rather than
        // refusing: its name is itself something the owner keeps back, and a
        // 403 would confirm it.
        let role = visible_role(&self.sql, Access::from_request(&req))?;
        if role < Role::Read {
            return Response::error("Not Found", 404);
        }
        let viewer = web::Viewer {
            name: actor_name,
            role,
        };

        match (req.method(), action) {
            // -- Git smart HTTP protocol --
            (Method::Get, "info") if parts.get(3) == Some(&"refs") => {
                let service = url
                    .query_pairs()
                    .find(|(k, _)| k == "service")
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default();
                match service.as_str() {
                    "git-receive-pack" => {
                        if let Some(resp) = require_role(&actor, role, Role::Write) {
                            return resp;
                        }
                        // Refuse at advertisement time so git reports the reason
                        // before spending a pack upload on a doomed push.
                        if let Some(reason) =
                            mirror::push_rejection(&self.sql, &actor_scopes(&actor))?
                        {
                            return Response::error(format!("Forbidden: {}", reason), 403);
                        }
                        self.advertise_refs("git-receive-pack")
                    }
                    "git-upload-pack" => self.advertise_refs("git-upload-pack"),
                    _ => Response::error("Unsupported service", 403),
                }
            }
            (Method::Post, "git-receive-pack") => {
                if let Some(resp) = require_role(&actor, role, Role::Write) {
                    return resp;
                }
                if let Some(reason) = mirror::push_rejection(&self.sql, &actor_scopes(&actor))? {
                    return Response::error(format!("Forbidden: {}", reason), 403);
                }
                let body = req.bytes().await?;
                let refs_before = ci::ref_snapshot(&self.sql)?;
                let resp = git::handle_receive_pack(&self.sql, &body)?;
                // On successful push, register the repo in the REGISTRY KV so
                // the owner profile page can list it. Best-effort: never fail the push.
                if resp.status_code() == 200 {
                    // The mirror agent forwards the upstream's visibility from
                    // its signed OIDC claim. Absent for ordinary pushes, which
                    // leaves whatever the repo already had.
                    if let Ok(Some(vis)) = req.headers().get("X-Ripgit-Repo-Visibility") {
                        // A mirror's upstream is public or not; GitHub's own
                        // "internal" means a GitHub org, not this one.
                        let vis = if vis == "public" {
                            Visibility::Public
                        } else {
                            Visibility::Private
                        };
                        store::set_config(&self.sql, CFG_VISIBILITY, vis.as_str())?;
                    }

                    // Visibility is mirrored into the registry so the owner
                    // profile can filter without waking every repo to ask.
                    let key = format!("repo:{}/{}", owner, repo_name);
                    let value = visibility(&self.sql)?.as_str();
                    if let Ok(kv) = self.env.kv("REGISTRY") {
                        if let Ok(builder) = kv.put(&key, value) {
                            let _ = builder.execute().await;
                        }
                    }

                    // Start CI for branches that moved. Mirrors do not run CI:
                    // upstream already did, and a mirror's job is to keep up.
                    // Best-effort like the registry write; never fail the push.
                    let is_mirror = actor.as_ref().is_some_and(|a| a.kind == "mirror");
                    if !is_mirror {
                        let moved = ci::moved_refs(&refs_before, &ci::ref_snapshot(&self.sql)?);
                        let pusher = actor_name.unwrap_or("");
                        if let Err(e) =
                            ci::trigger_push(&self.env, &self.sql, owner, repo_name, &moved, pusher).await
                        {
                            console_error!("ci trigger for {}/{} failed: {}", owner, repo_name, e);
                        }
                    }
                }
                Ok(resp)
            }
            (Method::Post, "git-upload-pack") => {
                let body = req.bytes().await?;
                git::handle_upload_pack(&self.sql, &body)
            }

            // -- Source archive (the tree at a commit, as tar) --
            (Method::Get, "archive") if parts.len() == 4 && is_hex40(parts[3]) => {
                match ci::archive(&self.sql, parts[3])? {
                    Some(tar) => {
                        let mut resp = Response::from_bytes(tar)?;
                        let headers = resp.headers_mut();
                        headers.set("Content-Type", "application/x-tar")?;
                        headers.set(
                            "Content-Disposition",
                            &format!("attachment; filename=\"{}-{}.tar\"", repo_name, &parts[3][..7]),
                        )?;
                        Ok(resp)
                    }
                    None => Response::error("Not Found", 404),
                }
            }

            // -- CI --
            (Method::Post, "ci") if parts.get(3) == Some(&"report") => {
                // Only ripgit-ci, and only about the repo its run belongs to.
                let path = format!("{}/{}", owner, repo_name).to_lowercase();
                let is_runner = actor.as_ref().is_some_and(|a| {
                    a.kind == "ci" && a.repo.as_deref().map(str::to_lowercase) == Some(path.clone())
                });
                if !is_runner {
                    return Response::error("Forbidden", 403);
                }
                let report: ci::Report = match req.json().await {
                    Ok(report) => report,
                    Err(e) => return Response::error(format!("bad report: {}", e), 400),
                };
                match ci::apply_report(&self.sql, report)? {
                    Ok(()) => Response::ok("ok"),
                    Err(message) => Response::error(message, 400),
                }
            }
            (Method::Get, "actions") => self.handle_actions(&req, &parts, viewer).await,

            // -- Artifacts mirror (owner only) --
            (Method::Post, "artifacts") if parts.get(3) == Some(&"link") => {
                if let Some(resp) = require_role(&actor, role, Role::Admin) {
                    return resp;
                }
                if !artifacts::can_manage(&self.env, actor.as_ref()).await? {
                    return Response::error("Artifacts management is restricted to deployment operators", 403);
                }
                if !artifacts_same_origin(&req)? { return Response::error("Invalid origin", 403); }
                self.link_artifacts(&mut req).await
            }
            (Method::Post, "artifacts") if parts.get(3) == Some(&"sync") => {
                if let Some(resp) = require_role(&actor, role, Role::Admin) {
                    return resp;
                }
                if !artifacts::can_manage(&self.env, actor.as_ref()).await? {
                    return Response::error("Artifacts management is restricted to deployment operators", 403);
                }
                if !artifacts_same_origin(&req)? { return Response::error("Invalid origin", 403); }
                let response = self.sync_artifacts().await?;
                if response.status_code() < 400 {
                    self.register_artifact(owner, repo_name).await?;
                }
                Ok(response)
            }
            (Method::Get, "artifacts") => {
                let repo = store::get_config(&self.sql, artifacts::CFG_REPO)?;
                let remote = store::get_config(&self.sql, artifacts::CFG_REMOTE)?;
                let last = store::get_config(&self.sql, artifacts::CFG_LAST_SYNC)?;
                Response::from_json(&serde_json::json!({
                    "linked": repo.is_some() || remote.is_some(),
                    "mode": repo.as_ref().map(|_| "bound").or(remote.as_ref().map(|_| "external")),
                    "repo": repo,
                    "remote": remote,
                    "last_sync": last,
                }))
            }

            // -- Mirror failover (owner only) --
            (Method::Post, "mirror") if parts.get(3) == Some(&"promote") => {
                if let Some(resp) = require_role(&actor, role, Role::Admin) {
                    return resp;
                }
                self.promote_mirror().await
            }
            (Method::Post, "mirror") if parts.get(3) == Some(&"demote") => {
                if let Some(resp) = require_role(&actor, role, Role::Admin) {
                    return resp;
                }
                store::set_config(&self.sql, mirror::CFG_PROMOTED, "0")?;
                Response::from_json(&serde_json::json!({ "promoted": false }))
            }
            (Method::Get, "mirror") => {
                let fork_point = store::get_config(&self.sql, mirror::CFG_FORK_POINT)?
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
                Response::from_json(&serde_json::json!({
                    "mirrored": mirror::is_mirrored(&self.sql)?,
                    "promoted": mirror::is_promoted(&self.sql)?,
                    "upstream": store::get_config(&self.sql, mirror::CFG_GITHUB_REMOTE)?,
                    "last_sync": store::get_config(&self.sql, artifacts::CFG_LAST_SYNC)?,
                    "fork_point": fork_point,
                }))
            }

            // -- GitHub mirror (owner only) --
            (Method::Post, "mirror") if parts.get(3) == Some(&"link") => {
                if let Some(resp) = require_role(&actor, role, Role::Admin) {
                    return resp;
                }
                self.link_github(&mut req, owner, repo_name).await
            }
            (Method::Post, "mirror") if parts.get(3) == Some(&"sync") => {
                if let Some(resp) = require_role(&actor, role, Role::Admin) {
                    return resp;
                }
                self.sync_github().await
            }

            // -- Delete all data (owner only) --
            (Method::Delete, "") => {
                if let Some(resp) = require_role(&actor, role, Role::Admin) {
                    return resp;
                }
                self.state.storage().delete_all().await?;
                Response::ok("deleted")
            }

            // -- JSON API (always JSON) --
            (Method::Get, "refs") => api::handle_refs(&self.sql),
            (Method::Get, "file") => api::handle_file(&self.sql, &url),
            (Method::Get, "search") => api::handle_search(&self.sql, &url),
            (Method::Get, "stats") => api::handle_stats(&self.sql),

            // -- Diff / commit history --
            (Method::Get, "diff") => {
                let sha = parts.get(3).unwrap_or(&"");
                let selection = match negotiate_or_response(
                    &req,
                    &[
                        Representation::Json,
                        Representation::Html,
                        Representation::Markdown,
                    ],
                    Representation::Json,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };
                match selection.representation() {
                    Representation::Json => {
                        finalize_negotiated(diff::handle_diff(&self.sql, sha, &url), &selection)
                    }
                    Representation::Html => finalize_negotiated(
                        web::page_commit(&self.sql, owner, repo_name, sha, viewer),
                        &selection,
                    ),
                    Representation::Markdown => finalize_negotiated(
                        web::page_diff_markdown(&self.sql, owner, repo_name, sha, &selection),
                        &selection,
                    ),
                }
            }
            (Method::Get, "compare") => {
                let spec = parts.get(3).unwrap_or(&"");
                diff::handle_compare(&self.sql, spec, &url)
            }

            (Method::Get, "log") => {
                let selection = match negotiate_or_response(
                    &req,
                    &[
                        Representation::Json,
                        Representation::Html,
                        Representation::Markdown,
                    ],
                    Representation::Json,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };
                match selection.representation() {
                    Representation::Json => {
                        finalize_negotiated(api::handle_log(&self.sql, &url), &selection)
                    }
                    Representation::Html => finalize_negotiated(
                        web::page_log(&self.sql, owner, repo_name, &url, viewer),
                        &selection,
                    ),
                    Representation::Markdown => finalize_negotiated(
                        web::page_log_markdown(&self.sql, owner, repo_name, &url, &selection),
                        &selection,
                    ),
                }
            }
            (Method::Get, "commit") => {
                let hash = parts.get(3).unwrap_or(&"");
                let selection = match negotiate_or_response(
                    &req,
                    &[
                        Representation::Json,
                        Representation::Html,
                        Representation::Markdown,
                    ],
                    Representation::Json,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };
                match selection.representation() {
                    Representation::Json => {
                        finalize_negotiated(api::handle_commit(&self.sql, hash), &selection)
                    }
                    Representation::Html => finalize_negotiated(
                        web::page_commit(&self.sql, owner, repo_name, hash, viewer),
                        &selection,
                    ),
                    Representation::Markdown => finalize_negotiated(
                        web::page_commit_markdown(&self.sql, owner, repo_name, hash, &selection),
                        &selection,
                    ),
                }
            }
            // tree/blob by 40-hex hash → JSON API
            (Method::Get, "tree") if is_hex40(parts.get(3).unwrap_or(&"")) => {
                api::handle_tree(&self.sql, parts.get(3).unwrap_or(&""))
            }
            (Method::Get, "blob") if is_hex40(parts.get(3).unwrap_or(&"")) => {
                api::handle_blob(&self.sql, parts.get(3).unwrap_or(&""))
            }

            // -- Web UI --
            (Method::Get, "") => {
                let selection = match negotiate_or_response(
                    &req,
                    &[Representation::Html, Representation::Markdown],
                    Representation::Html,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };
                match selection.representation() {
                    Representation::Html => finalize_negotiated(
                        web::page_home(&self.sql, owner, repo_name, &url, viewer),
                        &selection,
                    ),
                    Representation::Markdown => finalize_negotiated(
                        web::page_home_markdown(
                            &self.sql, owner, repo_name, &url, viewer, &selection,
                        ),
                        &selection,
                    ),
                    Representation::Json => unreachable!(),
                }
            }
            (Method::Get, "commits") => {
                let selection = match negotiate_or_response(
                    &req,
                    &[Representation::Html, Representation::Markdown],
                    Representation::Html,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };
                match selection.representation() {
                    Representation::Html => finalize_negotiated(
                        web::page_log(&self.sql, owner, repo_name, &url, viewer),
                        &selection,
                    ),
                    Representation::Markdown => finalize_negotiated(
                        web::page_log_markdown(&self.sql, owner, repo_name, &url, &selection),
                        &selection,
                    ),
                    Representation::Json => unreachable!(),
                }
            }
            (Method::Get, "tree") => {
                let selection = match negotiate_or_response(
                    &req,
                    &[Representation::Html, Representation::Markdown],
                    Representation::Html,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };
                let ref_name = parts.get(3).unwrap_or(&"main");
                let sub_path = if parts.len() > 4 {
                    parts[4..].join("/")
                } else {
                    String::new()
                };
                match selection.representation() {
                    Representation::Html => finalize_negotiated(
                        web::page_tree(
                            &self.sql, owner, repo_name, ref_name, &sub_path, viewer,
                        ),
                        &selection,
                    ),
                    Representation::Markdown => finalize_negotiated(
                        web::page_tree_markdown(
                            &self.sql, owner, repo_name, ref_name, &sub_path, &selection,
                        ),
                        &selection,
                    ),
                    Representation::Json => unreachable!(),
                }
            }
            (Method::Get, "blob") => {
                let selection = match negotiate_or_response(
                    &req,
                    &[Representation::Html, Representation::Markdown],
                    Representation::Html,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };
                let ref_name = parts.get(3).unwrap_or(&"main");
                let sub_path = if parts.len() > 4 {
                    parts[4..].join("/")
                } else {
                    String::new()
                };
                match selection.representation() {
                    Representation::Html => finalize_negotiated(
                        web::page_blob(
                            &self.sql, owner, repo_name, ref_name, &sub_path, viewer,
                        ),
                        &selection,
                    ),
                    Representation::Markdown => finalize_negotiated(
                        web::page_blob_markdown(
                            &self.sql, owner, repo_name, ref_name, &sub_path, &selection,
                        ),
                        &selection,
                    ),
                    Representation::Json => unreachable!(),
                }
            }
            (Method::Get, "search-ui") => {
                let selection = match negotiate_or_response(
                    &req,
                    &[Representation::Html, Representation::Markdown],
                    Representation::Html,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };
                match selection.representation() {
                    Representation::Html => finalize_negotiated(
                        web::page_search(&self.sql, owner, repo_name, &url, viewer),
                        &selection,
                    ),
                    Representation::Markdown => finalize_negotiated(
                        web::page_search_markdown(&self.sql, owner, repo_name, &url, &selection),
                        &selection,
                    ),
                    Representation::Json => unreachable!(),
                }
            }
            (Method::Get, "settings") => {
                if let Some(resp) = require_role(&actor, role, Role::Admin) {
                    return resp;
                }
                let selection = match negotiate_or_response(
                    &req,
                    &[Representation::Html, Representation::Markdown],
                    Representation::Html,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };
                let access = authz::load_repo_access(&self.env, owner, repo_name).await?;
                let vis = visibility(&self.sql)?.as_str();
                match selection.representation() {
                    Representation::Html => finalize_negotiated(
                        web::page_settings(&self.sql, owner, repo_name, viewer, vis, &access),
                        &selection,
                    ),
                    Representation::Markdown => finalize_negotiated(
                        web::page_settings_markdown(
                            &self.sql, owner, repo_name, vis, &access, &selection,
                        ),
                        &selection,
                    ),
                    Representation::Json => unreachable!(),
                }
            }
            (Method::Post, "settings") => {
                if let Some(resp) = require_role(&actor, role, Role::Admin) {
                    return resp;
                }
                let sub = parts.get(3).copied().unwrap_or("");
                self.handle_settings_action(owner, repo_name, sub, &url, req)
                    .await
            }
            (Method::Get, "raw") => {
                let ref_name = parts.get(3).unwrap_or(&"main");
                let sub_path = if parts.len() > 4 {
                    parts[4..].join("/")
                } else {
                    String::new()
                };
                web::serve_raw(&self.sql, ref_name, &sub_path)
            }

            // -- Issues --
            (Method::Get, "issues") => {
                let sub = parts.get(3).copied().unwrap_or("");
                if sub == "new" && actor.is_none() {
                    return unauthorized_401();
                }
                let issue_number = if sub.is_empty() || sub == "new" {
                    None
                } else {
                    match sub.parse::<i64>() {
                        Ok(num) => Some(num),
                        Err(_) => return Response::error("Not Found", 404),
                    }
                };

                let selection = match negotiate_or_response(
                    &req,
                    &[Representation::Html, Representation::Markdown],
                    Representation::Html,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };

                match selection.representation() {
                    Representation::Html => match (sub, issue_number) {
                        ("", _) => finalize_negotiated(
                            issues_web::page_issues_list(
                                &self.sql, owner, repo_name, &url, viewer,
                            ),
                            &selection,
                        ),
                        ("new", _) => finalize_negotiated(
                            issues_web::page_new_issue(&self.sql, owner, repo_name, viewer),
                            &selection,
                        ),
                        (_, Some(num)) => finalize_negotiated(
                            issues_web::page_issue_detail(
                                &self.sql, owner, repo_name, num, viewer,
                            ),
                            &selection,
                        ),
                        _ => Response::error("Not Found", 404),
                    },
                    Representation::Markdown => match (sub, issue_number) {
                        ("", _) => finalize_negotiated(
                            issues_web::page_issues_list_markdown(
                                &self.sql, owner, repo_name, &url, viewer, &selection,
                            ),
                            &selection,
                        ),
                        ("new", _) => finalize_negotiated(
                            issues_web::page_new_issue_markdown(
                                &self.sql, owner, repo_name, viewer, &selection,
                            ),
                            &selection,
                        ),
                        (_, Some(num)) => finalize_negotiated(
                            issues_web::page_issue_detail_markdown(
                                &self.sql, owner, repo_name, num, viewer, &selection,
                            ),
                            &selection,
                        ),
                        _ => Response::error("Not Found", 404),
                    },
                    Representation::Json => unreachable!(),
                }
            }
            (Method::Post, "issues") => {
                if actor.is_none() {
                    return unauthorized_401();
                }
                let sub3 = parts.get(3).copied().unwrap_or("");
                let sub4 = parts.get(4).copied().unwrap_or("");
                let aname = actor_name.unwrap_or("");
                self.handle_issue_action(owner, repo_name, sub3, sub4, "issues", aname, role, &url, req)
                    .await
            }

            // -- Pull requests --
            (Method::Get, "pulls") => {
                let sub = parts.get(3).copied().unwrap_or("");
                if sub == "new" && actor.is_none() {
                    return unauthorized_401();
                }
                let pull_number = if sub.is_empty() || sub == "new" {
                    None
                } else {
                    match sub.parse::<i64>() {
                        Ok(num) => Some(num),
                        Err(_) => return Response::error("Not Found", 404),
                    }
                };

                let selection = match negotiate_or_response(
                    &req,
                    &[Representation::Html, Representation::Markdown],
                    Representation::Html,
                ) {
                    Ok(selection) => selection,
                    Err(resp) => return resp,
                };

                match selection.representation() {
                    Representation::Html => match (sub, pull_number) {
                        ("", _) => finalize_negotiated(
                            issues_web::page_pulls_list(
                                &self.sql, owner, repo_name, &url, viewer,
                            ),
                            &selection,
                        ),
                        ("new", _) => finalize_negotiated(
                            issues_web::page_new_pull(
                                &self.sql, owner, repo_name, &url, viewer,
                            ),
                            &selection,
                        ),
                        (_, Some(num)) => finalize_negotiated(
                            issues_web::page_issue_detail(
                                &self.sql, owner, repo_name, num, viewer,
                            ),
                            &selection,
                        ),
                        _ => Response::error("Not Found", 404),
                    },
                    Representation::Markdown => match (sub, pull_number) {
                        ("", _) => finalize_negotiated(
                            issues_web::page_pulls_list_markdown(
                                &self.sql, owner, repo_name, &url, viewer, &selection,
                            ),
                            &selection,
                        ),
                        ("new", _) => finalize_negotiated(
                            issues_web::page_new_pull_markdown(
                                &self.sql, owner, repo_name, &url, viewer, &selection,
                            ),
                            &selection,
                        ),
                        (_, Some(num)) => finalize_negotiated(
                            issues_web::page_issue_detail_markdown(
                                &self.sql, owner, repo_name, num, viewer, &selection,
                            ),
                            &selection,
                        ),
                        _ => Response::error("Not Found", 404),
                    },
                    Representation::Json => unreachable!(),
                }
            }
            (Method::Post, "pulls") => {
                if actor.is_none() {
                    return unauthorized_401();
                }
                let sub3 = parts.get(3).copied().unwrap_or("");
                let sub4 = parts.get(4).copied().unwrap_or("");
                let aname = actor_name.unwrap_or("");
                self.handle_issue_action(owner, repo_name, sub3, sub4, "pulls", aname, role, &url, req)
                    .await
            }

            // -- Admin endpoints (owner only) --
            (Method::Put, "admin") => {
                if let Some(resp) = require_role(&actor, role, Role::Admin) {
                    return resp;
                }
                let sub = parts.get(3).unwrap_or(&"");
                match *sub {
                    "set-ref" => {
                        let name = url
                            .query_pairs()
                            .find(|(k, _)| k == "name")
                            .map(|(_, v)| v.to_string());
                        let hash = url
                            .query_pairs()
                            .find(|(k, _)| k == "hash")
                            .map(|(_, v)| v.to_string());
                        match (name, hash) {
                            (Some(n), Some(h)) => {
                                self.sql.exec(
                                    "INSERT INTO refs (name, commit_hash) VALUES (?, ?)
                                     ON CONFLICT(name) DO UPDATE SET commit_hash = ?",
                                    vec![
                                        SqlStorageValue::from(n.clone()),
                                        SqlStorageValue::from(h.clone()),
                                        SqlStorageValue::from(h.clone()),
                                    ],
                                )?;
                                Response::ok(format!("{} -> {}", n, h))
                            }
                            _ => Response::ok("need ?name=refs/heads/main&hash=abc123"),
                        }
                    }
                    "config" => {
                        let key = url
                            .query_pairs()
                            .find(|(k, _)| k == "key")
                            .map(|(_, v)| v.to_string());
                        let value = url
                            .query_pairs()
                            .find(|(k, _)| k == "value")
                            .map(|(_, v)| v.to_string());
                        match (key, value) {
                            (Some(k), Some(v)) => {
                                store::set_config(&self.sql, &k, &v)?;
                                Response::ok(format!("{} = {}", k, v))
                            }
                            (Some(k), None) => {
                                let v = store::get_config(&self.sql, &k)?;
                                Response::ok(v.unwrap_or_else(|| "(not set)".to_string()))
                            }
                            _ => Response::ok("need ?key=name[&value=val]"),
                        }
                    }
                    "rebuild-fts" => {
                        let default_ref = store::get_config(&self.sql, "default_branch")?
                            .unwrap_or_else(|| "refs/heads/main".to_string());
                        #[derive(serde::Deserialize)]
                        struct RefRow {
                            commit_hash: String,
                        }
                        let rows: Vec<RefRow> = self
                            .sql
                            .exec(
                                "SELECT commit_hash FROM refs WHERE name = ?",
                                vec![SqlStorageValue::from(default_ref)],
                            )?
                            .to_array()?;
                        if let Some(row) = rows.first() {
                            store::rebuild_fts_index(&self.sql, &row.commit_hash)?;
                            Response::ok("fts rebuilt")
                        } else {
                            Response::ok("no default branch ref found")
                        }
                    }
                    "rebuild-graph" => {
                        // Bulk rebuild commit graph using INSERT...SELECT per level.
                        // ~14 SQL calls for any repo size.
                        self.sql.exec("DELETE FROM commit_graph", None)?;

                        // Level 0: direct first-parent
                        self.sql.exec(
                            "INSERT INTO commit_graph (commit_hash, level, ancestor_hash)
                             SELECT cp.commit_hash, 0, cp.parent_hash
                             FROM commit_parents cp WHERE cp.ordinal = 0",
                            None,
                        )?;

                        let mut level: i64 = 1;
                        loop {
                            let prev = level - 1;
                            let result = self.sql.exec(
                                &format!(
                                    "INSERT INTO commit_graph (commit_hash, level, ancestor_hash)
                                     SELECT cg.commit_hash, {}, cg2.ancestor_hash
                                     FROM commit_graph cg
                                     JOIN commit_graph cg2
                                       ON cg2.commit_hash = cg.ancestor_hash AND cg2.level = {}
                                     WHERE cg.level = {}",
                                    level, prev, prev
                                ),
                                None,
                            )?;
                            if result.rows_written() == 0 {
                                break;
                            }
                            level += 1;
                        }

                        Response::ok(format!("commit graph rebuilt ({} levels)", level))
                    }
                    "rebuild-fts-commits" => {
                        // Bulk rebuild fts_commits from all commits
                        self.sql.exec("DELETE FROM fts_commits", None)?;
                        self.sql.exec(
                            "INSERT INTO fts_commits (hash, message, author)
                             SELECT hash, message, author FROM commits",
                            None,
                        )?;
                        #[derive(serde::Deserialize)]
                        struct Count {
                            n: i64,
                        }
                        let rows: Vec<Count> = self
                            .sql
                            .exec("SELECT COUNT(*) AS n FROM fts_commits", None)?
                            .to_array()?;
                        let n = rows.first().map(|r| r.n).unwrap_or(0);
                        Response::ok(format!("fts_commits rebuilt ({} entries)", n))
                    }
                    _ => Response::error("unknown admin action", 404),
                }
            }

            _ => Response::error("Not Found", 404),
        }
    }
}

// ---------------------------------------------------------------------------
// Git protocol helpers
// ---------------------------------------------------------------------------

impl Repository {
    /// GET /:owner/:repo/actions[/:n[/logs/:job/:idx]]
    async fn handle_actions(&self, req: &Request, parts: &[&str], viewer: web::Viewer<'_>) -> Result<Response> {
        let owner = parts[0];
        let repo_name = parts[1];
        let (default_branch, _) = web::resolve_default_branch(&self.sql)?;

        // Raw step log: /actions/:n/logs/:job/:idx
        if parts.len() == 7 && parts[4] == "logs" {
            let Ok(number) = parts[3].parse::<i64>() else {
                return Response::error("Not Found", 404);
            };
            let job = urlencoding_decode(parts[5]);
            let Ok(idx) = parts[6].parse::<i64>() else {
                return Response::error("Not Found", 404);
            };
            let Some(step) = ci::steps(&self.sql, number)?
                .into_iter()
                .find(|s| s.job == job && s.idx == idx && !s.log_key.is_empty())
            else {
                return Response::error("Not Found", 404);
            };
            return match self.read_log(&step.log_key).await? {
                Some(text) => {
                    let mut resp = Response::ok(text)?;
                    resp.headers_mut().set("Content-Type", "text/plain; charset=utf-8")?;
                    Ok(resp)
                }
                None => Response::error("log not available", 404),
            };
        }

        let selection = match negotiate_or_response(
            req,
            &[Representation::Html, Representation::Markdown],
            Representation::Html,
        ) {
            Ok(selection) => selection,
            Err(resp) => return resp,
        };

        match parts.get(3).copied().filter(|s| !s.is_empty()) {
            None => {
                let runs = ci::list_runs(&self.sql, 100)?;
                match selection.representation() {
                    Representation::Markdown => finalize_negotiated(
                        web::page_actions_markdown(owner, repo_name, &runs, &selection),
                        &selection,
                    ),
                    _ => finalize_negotiated(
                        web::page_actions(owner, repo_name, &default_branch, &runs, viewer),
                        &selection,
                    ),
                }
            }
            Some(n) => {
                let Some(run) = n.parse::<i64>().ok().and_then(|n| ci::get_run(&self.sql, n).ok().flatten())
                else {
                    return Response::error("Not Found", 404);
                };
                let jobs = ci::jobs(&self.sql, run.number)?;
                let steps = ci::steps(&self.sql, run.number)?;
                // Show why a step failed without a click: the end of its log.
                let mut tails = Vec::new();
                for step in steps
                    .iter()
                    .filter(|s| matches!(s.status.as_str(), "failure" | "error") && !s.log_key.is_empty())
                {
                    if let Some(text) = self.read_log(&step.log_key).await? {
                        tails.push(web::LogTail {
                            job: step.job.clone(),
                            idx: step.idx,
                            text: last_lines(&text, 100),
                        });
                    }
                }
                match selection.representation() {
                    Representation::Markdown => finalize_negotiated(
                        web::page_run_markdown(owner, repo_name, &run, &jobs, &steps, &tails, &selection),
                        &selection,
                    ),
                    _ => finalize_negotiated(
                        web::page_run(owner, repo_name, &default_branch, &run, &jobs, &steps, &tails, viewer),
                        &selection,
                    ),
                }
            }
        }
    }

    /// A step log from the CI_LOGS bucket, or None when the bucket is not
    /// bound or the object is gone (logs expire).
    async fn read_log(&self, key: &str) -> Result<Option<String>> {
        let Ok(bucket) = self.env.bucket("CI_LOGS") else {
            return Ok(None);
        };
        match bucket.get(key).execute().await? {
            Some(object) => match object.body() {
                Some(body) => Ok(Some(body.text().await?)),
                None => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// Handle POST /:owner/:repo/{issues,pulls}/:sub3/:sub4
    async fn handle_issue_action(
        &self,
        owner: &str,
        repo_name: &str,
        sub3: &str,       // "" | "new" | "<number>"
        sub4: &str,       // "" | "comment" | "close" | "reopen" | "merge"
        kind_url: &str,   // "issues" | "pulls"
        actor_name: &str, // already validated non-empty by caller
        role: Role,
        req_url: &Url,    // for building absolute redirect URLs
        mut req: Request,
    ) -> Result<Response> {
        let kind = if kind_url == "pulls" { "pr" } else { "issue" };

        // POST /{issues|pulls}  →  create
        if sub3.is_empty() {
            let body = req.text().await?;
            let form = issues::parse_form(&body);
            let title = form
                .get("title")
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let body_text = form.get("body").cloned().unwrap_or_default();

            if title.is_empty() {
                return Response::error("title is required", 400);
            }

            if kind == "pr" {
                let source = form.get("source").cloned().unwrap_or_default();
                let target = form.get("target").cloned().unwrap_or_default();
                if source.is_empty() || target.is_empty() {
                    return Response::error("source and target branches are required", 400);
                }
                if source == target {
                    return Response::error("source and target branches must differ", 400);
                }
                let source_ref = format!("refs/heads/{}", source);
                let source_hash = match api::resolve_ref(&self.sql, &source_ref)? {
                    Some(h) => Some(h),
                    None => return Response::error("source branch not found", 404),
                };
                let number = issues::create_issue(
                    &self.sql,
                    kind,
                    &title,
                    &body_text,
                    actor_name,
                    actor_name,
                    Some(&source),
                    Some(&target),
                    source_hash.as_deref(),
                )?;
                return make_redirect(
                    req_url,
                    &format!("/{}/{}/{}/{}", owner, repo_name, kind_url, number),
                );
            } else {
                let number = issues::create_issue(
                    &self.sql, kind, &title, &body_text, actor_name, actor_name, None, None, None,
                )?;
                return make_redirect(
                    req_url,
                    &format!("/{}/{}/{}/{}", owner, repo_name, kind_url, number),
                );
            }
        }

        // POST /{issues|pulls}/:n/...
        let number: i64 = match sub3.parse() {
            Ok(n) => n,
            Err(_) => return Response::error("Not Found", 404),
        };

        match sub4 {
            "comment" => {
                let body = req.text().await?;
                let form = issues::parse_form(&body);
                let comment_body = form.get("body").cloned().unwrap_or_default();
                let issue = issues::get_issue(&self.sql, number)?
                    .ok_or_else(|| Error::RustError("not found".into()))?;
                issues::create_comment(&self.sql, issue.id, &comment_body, actor_name, actor_name)?;
            }
            "close" => {
                issues::set_issue_state(&self.sql, number, "closed", actor_name, role >= Role::Triage)?;
            }
            "reopen" => {
                issues::set_issue_state(&self.sql, number, "open", actor_name, role >= Role::Triage)?;
            }
            "merge" => {
                if role < Role::Write {
                    return Response::error("Forbidden: merging requires write access", 403);
                }
                let target_branch =
                    issues::get_issue(&self.sql, number)?.and_then(|issue| issue.target_branch);
                match issues::merge_pr(&self.sql, number, actor_name) {
                    Ok(merge_hash) => {
                        if let Some(target_branch) = target_branch {
                            if let Some(default_ref) =
                                store::get_config(&self.sql, "default_branch")?
                            {
                                if default_ref == format!("refs/heads/{}", target_branch) {
                                    let _ = store::rebuild_fts_index(&self.sql, &merge_hash);
                                }
                            }
                        }
                    }
                    Err(e) => return Response::error(&e.to_string(), 409),
                }
            }
            _ => return Response::error("Not Found", 404),
        }

        make_redirect(
            req_url,
            &format!("/{}/{}/{}/{}", owner, repo_name, kind_url, number),
        )
    }

    /// Handle POST /:owner/:repo/settings/:action — all owner-only mutations.
    async fn handle_settings_action(
        &self,
        owner: &str,
        repo_name: &str,
        action: &str,
        req_url: &Url,
        mut req: Request,
    ) -> Result<Response> {
        let settings_path = format!("/{}/{}/settings", owner, repo_name);

        let back = || -> Result<Response> { make_redirect(req_url, &settings_path) };

        match action {
            "visibility" => {
                let form = issues::parse_form(&req.text().await?);
                let requested = form.get("visibility").map(String::as_str).unwrap_or("");
                let vis = Visibility::parse(requested);
                if vis.as_str() != requested {
                    return Response::error("visibility must be public, internal, or private", 400);
                }
                if vis == Visibility::Internal
                    && !authz::load_repo_access(&self.env, owner, repo_name).await?.is_org
                {
                    return Response::error(
                        "internal visibility is only available to organization repos",
                        400,
                    );
                }
                store::set_config(&self.sql, CFG_VISIBILITY, vis.as_str())?;
                // Keep the owner profile listing in step, as a push would.
                if let Ok(kv) = self.env.kv("REGISTRY") {
                    let key = format!("repo:{}/{}", owner, repo_name);
                    if let Ok(builder) = kv.put(&key, vis.as_str()) {
                        let _ = builder.execute().await;
                    }
                }
                back()
            }

            "grant" => {
                let form = issues::parse_form(&req.text().await?);
                let field = |name: &str| form.get(name).cloned().unwrap_or_default();
                match authz::grant(
                    &self.env,
                    owner,
                    repo_name,
                    &field("kind"),
                    &field("grantee"),
                    &field("role"),
                )
                .await?
                {
                    Ok(()) => back(),
                    Err(message) => Response::error(message, 400),
                }
            }

            "revoke" => {
                let form = issues::parse_form(&req.text().await?);
                let field = |name: &str| form.get(name).cloned().unwrap_or_default();
                authz::revoke(&self.env, owner, repo_name, &field("kind"), &field("id")).await?;
                back()
            }

            "rebuild-graph" => {
                self.sql.exec("DELETE FROM commit_graph", None)?;
                self.sql.exec(
                    "INSERT INTO commit_graph (commit_hash, level, ancestor_hash)
                     SELECT cp.commit_hash, 0, cp.parent_hash
                     FROM commit_parents cp WHERE cp.ordinal = 0",
                    None,
                )?;
                let mut level: i64 = 1;
                loop {
                    let prev = level - 1;
                    let result = self.sql.exec(
                        &format!(
                            "INSERT INTO commit_graph (commit_hash, level, ancestor_hash)
                             SELECT cg.commit_hash, {level}, cg2.ancestor_hash
                             FROM commit_graph cg
                             JOIN commit_graph cg2
                               ON cg2.commit_hash = cg.ancestor_hash AND cg2.level = {prev}
                             WHERE cg.level = {prev}",
                            level = level,
                            prev = prev,
                        ),
                        None,
                    )?;
                    if result.rows_written() == 0 {
                        break;
                    }
                    level += 1;
                }
                back()
            }

            "rebuild-fts-commits" => {
                self.sql.exec("DELETE FROM fts_commits", None)?;
                self.sql.exec(
                    "INSERT INTO fts_commits (hash, message, author)
                     SELECT hash, message, author FROM commits",
                    None,
                )?;
                back()
            }

            "rebuild-fts" => {
                let default_ref = store::get_config(&self.sql, "default_branch")?
                    .unwrap_or_else(|| "refs/heads/main".to_string());
                #[derive(serde::Deserialize)]
                struct RefRow {
                    commit_hash: String,
                }
                let rows: Vec<RefRow> = self
                    .sql
                    .exec(
                        "SELECT commit_hash FROM refs WHERE name = ?",
                        vec![SqlStorageValue::from(default_ref)],
                    )?
                    .to_array()?;
                if let Some(row) = rows.first() {
                    store::rebuild_fts_index(&self.sql, &row.commit_hash)?;
                }
                back()
            }

            "default-branch" => {
                let body = req.text().await?;
                let branch = body
                    .split('&')
                    .find_map(|pair| {
                        let mut kv = pair.splitn(2, '=');
                        if kv.next() == Some("branch") {
                            kv.next().map(|v| v.replace('+', " "))
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let branch = branch.trim();
                if !branch.is_empty() {
                    store::set_config(&self.sql, "default_branch", branch)?;
                }
                back()
            }

            "delete" => {
                let body = req.text().await?;
                let confirm_val = body
                    .split('&')
                    .find_map(|pair| {
                        let mut kv = pair.splitn(2, '=');
                        if kv.next() == Some("confirm") {
                            kv.next().map(|v| {
                                // minimal URL decode for / (%2F) and spaces
                                v.replace('+', " ").replace("%2F", "/").replace("%2f", "/")
                            })
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                let expected = format!("{}/{}", owner, repo_name);
                if confirm_val.trim() == expected {
                    self.state.storage().delete_all().await?;
                    make_redirect(req_url, &format!("/{}/", owner))
                } else {
                    // Wrong confirmation — bounce back to settings
                    back()
                }
            }

            _ => Response::error("Not Found", 404),
        }
    }

    /// Ref advertisement for both receive-pack and upload-pack.
    /// Returns current refs in pkt-line format so git knows what we have.
    /// Link this repo to an Artifacts remote.
    ///
    /// Three shapes, in precedence order:
    ///   `{"repo": "name"}`                     — an existing repo in the bound namespace
    ///   `{"create": "name"}`                   — provision a new one
    ///   `{"import": "https://github.com/..."}` — import an external repo, then link it
    ///   `{"remote": "...", "token": "..."}`    — a repo outside this namespace
    ///
    /// The first three store only the repo name; tokens are minted per sync.
    async fn link_artifacts(&self, req: &mut Request) -> Result<Response> {
        #[derive(serde::Deserialize, Default)]
        struct LinkBody {
            repo: Option<String>,
            create: Option<String>,
            import: Option<String>,
            /// Target name for an import. Defaults to this repo's own name.
            name: Option<String>,
            branch: Option<String>,
            remote: Option<String>,
            token: Option<String>,
            binding: Option<String>,
        }

        let body: LinkBody = match req.json().await {
            Ok(body) => body,
            Err(_) => return Response::error("Invalid JSON", 400),
        };
        // Linking must never replace a populated repo or an existing upstream.
        if mirror::is_mirrored(&self.sql)?
            || !self.sql.exec("SELECT 1 FROM refs LIMIT 1", None)?.to_array::<serde_json::Value>()?.is_empty() {
            return Response::error("This repository already contains data or has an upstream. Choose a new local name, or sync the existing link.", 409);
        }
        let binding = body
            .binding
            .unwrap_or_else(|| artifacts::DEFAULT_BINDING.to_string());

        // Derive this repo's own name from the DO path for import defaults.
        let url = req.url()?;
        let own_name = url
            .path()
            .trim_start_matches('/')
            .split('/')
            .nth(1)
            .unwrap_or("repo")
            .to_string();

        // Binding errors carry the useful detail -- a missing binding, a name
        // already taken, a namespace the account cannot reach. Propagating them
        // as Err would collapse all of that into a bare 500.
        let linked_name = if let Some(repo) = body.repo {
            self.env.artifacts(&binding)?.get(&repo).await?.info().await?;
            repo
        } else if let Some(name) = body.create {
            match artifacts::create_repo(&self.env, &binding, &name).await {
                Ok(created) => created.name,
                Err(e) => return Response::error(format!("artifacts create failed: {}", e), 502),
            }
        } else if let Some(source) = body.import {
            let target = body.name.unwrap_or_else(|| own_name.clone());
            match artifacts::import_repo(
                &self.env,
                &binding,
                &source,
                &target,
                body.branch.as_deref(),
            )
            .await
            {
                Ok(imported) => imported.name,
                Err(e) => return Response::error(format!("artifacts import failed: {}", e), 502),
            }
        } else if let (Some(remote), Some(token)) = (body.remote, body.token) {
            // External repo: no binding to mint through, so the token is stored.
            store::set_config(&self.sql, artifacts::CFG_REMOTE, &remote)?;
            store::set_config(&self.sql, artifacts::CFG_TOKEN, &token)?;
            store::set_config(&self.sql, CFG_VISIBILITY, "private")?;
            let owner = url.path().trim_start_matches('/').split('/').next().unwrap_or("");
            self.register_artifact(owner, &own_name).await?;
            return Response::from_json(&serde_json::json!({
                "linked": true, "mode": "external", "remote": remote,
            }));
        } else {
            return Response::error(
                "provide one of: repo, create, import, or remote + token",
                400,
            );
        };

        // Another request may have populated this DO while the binding call awaited.
        if mirror::is_mirrored(&self.sql)?
            || !self.sql.exec("SELECT 1 FROM refs LIMIT 1", None)?.to_array::<serde_json::Value>()?.is_empty() {
            return Response::error("The destination changed while linking. Choose a new local repository name.", 409);
        }
        store::set_config(&self.sql, artifacts::CFG_REPO, &linked_name)?;
        store::set_config(&self.sql, artifacts::CFG_BINDING, &binding)?;
        // Artifacts may contain private code. New links start private.
        store::set_config(&self.sql, CFG_VISIBILITY, "private")?;
        let owner = url.path().trim_start_matches('/').split('/').next().unwrap_or("");
        self.register_artifact(owner, &own_name).await?;

        Response::from_json(&serde_json::json!({
            "linked": true, "mode": "bound", "repo": linked_name, "binding": binding,
        }))
    }

    async fn register_artifact(&self, owner: &str, repo: &str) -> Result<()> {
        let kv = self.env.kv("REGISTRY")?;
        kv.put(&format!("repo:{owner}/{repo}"), visibility(&self.sql)?.as_str())?.execute().await?;
        kv.put(&format!("artifact-link:{owner}/{repo}"), serde_json::json!({
            "local": repo,
            "repo": store::get_config(&self.sql, artifacts::CFG_REPO)?,
            "last_sync": store::get_config(&self.sql, artifacts::CFG_LAST_SYNC)?,
        }).to_string())?.execute().await?;
        Ok(())
    }

    /// Pull the linked Artifacts remote into local storage.
    async fn sync_artifacts(&self) -> Result<Response> {
        if mirror::is_promoted(&self.sql)? {
            return Response::error("This mirror accepts local writes. Reconcile it before syncing the upstream.", 409);
        }
        let (remote, auth) = match artifacts::resolve_source(&self.env, &self.sql).await {
            Ok(source) => source,
            Err(e) => return Response::error(format!("artifacts not reachable: {}", e), 502),
        };
        let report = match artifacts::sync(&self.sql, &remote, &auth).await {
            Ok(report) => report,
            Err(e) => return Response::error(format!("artifacts sync failed: {}", e), 502),
        };
        if !report.errors.is_empty() {
            return Ok(Response::from_json(&report)?.with_status(502));
        }
        store::set_config(
            &self.sql,
            artifacts::CFG_LAST_SYNC,
            &Date::now().to_string(),
        )?;
        Response::from_json(&report)
    }

    /// Accept local writes while upstream is unreachable.
    ///
    /// Deliberate rather than automatic: failing over on a flapping upstream
    /// would give two writable copies of the same repo and no way to tell which
    /// is authoritative.
    async fn promote_mirror(&self) -> Result<Response> {
        if !mirror::is_mirrored(&self.sql)? {
            return Response::error("this repo does not mirror an upstream", 409);
        }
        if !mirror::is_promoted(&self.sql)? {
            // Only on the transition, so re-promoting cannot move the base and
            // strand commits written since the first promotion.
            mirror::record_fork_point(&self.sql)?;
            store::set_config(&self.sql, mirror::CFG_PROMOTED, "1")?;
        }
        let fork_point = store::get_config(&self.sql, mirror::CFG_FORK_POINT)?
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
        Response::from_json(&serde_json::json!({
            "promoted": true,
            "fork_point": fork_point,
        }))
    }

    /// Record the GitHub upstream for this repo and enroll it in the sweep.
    async fn link_github(
        &self,
        req: &mut Request,
        owner: &str,
        repo_name: &str,
    ) -> Result<Response> {
        #[derive(serde::Deserialize, Default)]
        struct LinkBody {
            remote: Option<String>,
        }

        let body: LinkBody = req.json().await.unwrap_or_default();
        let Some(remote) = body.remote else {
            return Response::error("remote is required, e.g. https://github.com/owner/repo", 400);
        };

        store::set_config(&self.sql, mirror::CFG_GITHUB_REMOTE, &remote)?;

        // Enrollment lives in KV so the scheduled sweep can list repos without
        // waking every DO to ask whether it has an upstream.
        let key = format!("{}{}/{}", mirror::REGISTRY_PREFIX, owner, repo_name);
        if let Ok(kv) = self.env.kv("REGISTRY") {
            if let Ok(builder) = kv.put(&key, "1") {
                let _ = builder.execute().await;
            }
        }

        Response::from_json(&serde_json::json!({ "linked": true, "remote": remote }))
    }

    /// Pull this repo's GitHub upstream into local storage.
    async fn sync_github(&self) -> Result<Response> {
        // Surface configuration problems as text rather than letting the error
        // propagate: a bare Err from a DO becomes an opaque 500, which is how a
        // missing GITHUB_MIRROR_TOKEN reads as "INTERNAL SERVER ERROR" instead
        // of naming the secret that has to be set.
        let source = match mirror::github_source(&self.env, &self.sql) {
            Ok(source) => source,
            Err(e) => return Response::error(format!("mirror misconfigured: {}", e), 500),
        };
        let Some((remote, auth)) = source else {
            return Response::error("no GitHub upstream configured for this repo", 409);
        };
        // A promoted repo holds commits upstream has never seen. Syncing would
        // fast-forward them away, which is exactly the loss promotion prevents.
        if mirror::is_promoted(&self.sql)? {
            return Response::error(
                "repo is promoted to primary; demote it after reconciling with upstream",
                409,
            );
        }
        // An unreachable or rejecting upstream is not this worker's fault, and
        // the reason it gave is the only useful thing to report.
        let report = match artifacts::sync(&self.sql, &remote, &auth).await {
            Ok(report) => report,
            Err(e) => return Response::error(format!("upstream sync failed: {}", e), 502),
        };
        store::set_config(
            &self.sql,
            artifacts::CFG_LAST_SYNC,
            &Date::now().to_string(),
        )?;
        Response::from_json(&report)
    }

    fn advertise_refs(&self, service: &str) -> Result<Response> {
        let content_type = format!("application/x-{}-advertisement", service);

        // Collect current refs
        #[derive(serde::Deserialize)]
        struct RefRow {
            name: String,
            commit_hash: String,
        }
        let refs: Vec<RefRow> = self
            .sql
            .exec("SELECT name, commit_hash FROM refs", None)?
            .to_array()?;

        let mut body = Vec::new();

        // Service announcement
        let svc_line = format!("# service={}\n", service);
        pkt_line(&mut body, &svc_line);
        body.extend_from_slice(b"0000"); // flush

        // Build capabilities, including symref for HEAD.
        // upload-pack and receive-pack speak different capability sets.
        let default_branch = store::get_config(&self.sql, "default_branch")?
            .unwrap_or_else(|| "refs/heads/main".to_string());
        let caps = match service {
            "git-upload-pack" => format!(
                "multi_ack_detailed no-done ofs-delta side-band-64k no-progress symref=HEAD:{}",
                default_branch
            ),
            _ => format!(
                "report-status delete-refs ofs-delta side-band-64k quiet symref=HEAD:{}",
                default_branch
            ),
        };

        if refs.is_empty() {
            // Empty repo: advertise zero-id with capabilities
            let line = format!(
                "0000000000000000000000000000000000000000 capabilities^{{}}\0{}\n",
                caps
            );
            pkt_line(&mut body, &line);
        } else {
            // Find the default branch's commit for HEAD
            let head_hash = refs
                .iter()
                .find(|r| r.name == default_branch)
                .map(|r| r.commit_hash.clone());

            let mut first = true;

            // Advertise HEAD first (so git clone checks out the right branch)
            if let Some(ref hh) = head_hash {
                let line = format!("{} HEAD\0{}\n", hh, caps);
                pkt_line(&mut body, &line);
                first = false;
            }

            for r in refs.iter() {
                let line = if first {
                    first = false;
                    format!("{} {}\0{}\n", r.commit_hash, r.name, caps)
                } else {
                    format!("{} {}\n", r.commit_hash, r.name)
                };
                pkt_line(&mut body, &line);
            }
        }
        body.extend_from_slice(b"0000"); // flush

        let mut resp = Response::from_bytes(body)?;
        resp.headers_mut().set("Content-Type", &content_type)?;
        resp.headers_mut().set("Cache-Control", "no-cache")?;
        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// Pkt-line encoding
// ---------------------------------------------------------------------------

/// Append a pkt-line encoded string to the buffer.
/// Pkt-line format: 4 hex digits for total length (including the 4 digits),
/// followed by the payload.
fn pkt_line(buf: &mut Vec<u8>, data: &str) {
    let len = 4 + data.len();
    buf.extend_from_slice(format!("{:04x}", len).as_bytes());
    buf.extend_from_slice(data.as_bytes());
}

/// Check if a string is a 40-character hex SHA-1 hash.
/// Used to distinguish API calls (by hash) from web UI calls (by ref + path).
/// The last `n` lines of `text`.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Decode %XX escapes in one URL path segment. Unlike form decoding, `+` is
/// a literal plus. Invalid escapes pass through; invalid UTF-8 is replaced.
fn urlencoding_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// List the repos registered in the REGISTRY KV for `owner` that the viewer
/// may see. Keys are "repo:{owner}/{repo}"; values hold the repo's visibility,
/// mirrored there on push so this page need not wake every repo to ask.
///
/// Namespace admins see everything and org members also see internal repos.
/// A private repo someone was granted access to individually is not listed;
/// it is still reachable by URL.
///
/// Returns an empty list if the KV binding is unavailable or the list fails.
async fn list_repos(env: &Env, owner: &str, access: Access) -> Vec<String> {
    let prefix = format!("repo:{}/", owner);
    let kv = match env.kv("REGISTRY") {
        Ok(kv) => kv,
        Err(_) => return vec![],
    };
    let Ok(result) = kv.list().prefix(prefix.clone()).execute().await else {
        return vec![];
    };

    let mut repos = Vec::new();
    for key in result.keys {
        let name = key.name[prefix.len()..].to_string();
        if access.role >= Role::Admin {
            repos.push(name);
            continue;
        }
        let visible = match kv.get(&key.name).text().await {
            Ok(Some(v)) if v == "private" => false,
            Ok(Some(v)) if v == "internal" => access.org_member,
            // "public", and "1" from before visibility was tracked.
            _ => true,
        };
        if visible {
            repos.push(name);
        }
    }
    repos
}
