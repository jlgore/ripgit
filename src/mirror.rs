//! GitHub mirroring — the pull half.
//!
//! Repos are normally mirrored *into* ripgit by a GitHub Actions workflow that
//! authenticates with a short-lived OIDC-exchanged token (see
//! `examples/github-actions-mirror/`). That path is keyless and fires on every
//! push, but it stops working exactly when Actions does — which is one of the
//! outages this mirror exists to survive.
//!
//! So ripgit also pulls. A scheduled Workflow walks the enrolled repos and
//! fetches from GitHub directly, using the same smart-HTTP client that syncs
//! Artifacts repos. The two paths share no failure mode: one is pushed by
//! GitHub over Actions, the other is pulled by Cloudflare over git. A repo goes
//! stale only if both are down.
//!
//! Steps are durable: a fetch that fails is retried with backoff, and a repo
//! that fails permanently does not stop the others.

use crate::artifacts::RemoteAuth;
use serde::{Deserialize, Serialize};
use worker::*;

/// Config key holding the upstream GitHub clone URL for this repo.
pub const CFG_GITHUB_REMOTE: &str = "mirror_github_remote";

/// KV key prefix marking a repo as enrolled for the scheduled pull sweep.
///
/// The upstream URL itself lives in the repo's own DO config, but the sweep
/// needs the list without waking every DO to ask, so enrollment is mirrored
/// into the registry KV when the upstream is linked.
pub const REGISTRY_PREFIX: &str = "mirror:";

/// Worker secret holding the credential used to pull from GitHub.
///
/// This lives in Cloudflare rather than in GitHub, which is the whole point:
/// the OIDC push path exists so GitHub holds no ripgit secret, and this pull
/// path is the inverse — ripgit holds a GitHub secret. A fine-grained PAT or a
/// GitHub App installation token both work.
pub const GITHUB_TOKEN_SECRET: &str = "GITHUB_MIRROR_TOKEN";

/// Resolve the GitHub remote and credential for a repo, if it is enrolled.
pub fn github_source(env: &Env, sql: &SqlStorage) -> Result<Option<(String, RemoteAuth)>> {
    let Some(remote) = crate::store::get_config(sql, CFG_GITHUB_REMOTE)? else {
        return Ok(None);
    };
    let token = env
        .secret(GITHUB_TOKEN_SECRET)
        .map_err(|_| {
            Error::RustError(format!(
                "{} is not configured — set it with `wrangler secret put {}`",
                GITHUB_TOKEN_SECRET, GITHUB_TOKEN_SECRET
            ))
        })?
        .to_string();
    Ok(Some((remote, RemoteAuth::github(&token))))
}

// ---------------------------------------------------------------------------
// Workflow
// ---------------------------------------------------------------------------

/// Repos to sweep, as `owner/repo` paths within ripgit.
#[derive(Debug, Serialize, Deserialize)]
pub struct MirrorInput {
    pub repos: Vec<String>,
}

/// Outcome for a single repo. Recorded per repo so one bad remote does not
/// hide the state of the rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoOutcome {
    pub repo: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct MirrorOutput {
    pub results: Vec<RepoOutcome>,
    pub failed: usize,
}

#[workflow]
pub struct MirrorWorkflow {
    env: Env,
}

impl WorkflowEntrypoint for MirrorWorkflow {
    type Input = MirrorInput;
    type Output = MirrorOutput;

    fn new(_ctx: Context, env: Env) -> Self {
        Self { env }
    }

    async fn run(
        &self,
        event: WorkflowEvent<Self::Input>,
        step: WorkflowStep,
    ) -> Result<Self::Output> {
        let mut results: Vec<RepoOutcome> = Vec::new();

        for repo in event.payload.repos {
            let env = self.env.clone();
            let repo_for_step = repo.clone();

            // Retries absorb a GitHub blip or a DO that was mid-eviction. A repo
            // still failing after these is recorded and the sweep moves on --
            // one unreachable remote must not strand every other mirror.
            let outcome = step
                .do_(format!("sync:{}", repo), move |_| {
                    let env = env.clone();
                    let repo = repo_for_step.clone();
                    async move { sync_one(&env, &repo).await }
                })
                .config(
                    WorkflowStepConfig::new()
                        .retries(
                            WorkflowRetryConfig::new(3, "10 seconds")
                                .backoff(WorkflowBackoff::Exponential),
                        )
                        .timeout("5 minutes"),
                )
                .await;

            results.push(match outcome {
                Ok(outcome) => outcome,
                Err(e) => RepoOutcome {
                    repo,
                    ok: false,
                    detail: e.to_string(),
                },
            });
        }

        let failed = results.iter().filter(|r| !r.ok).count();
        Ok(MirrorOutput { results, failed })
    }
}

/// Ask one repo's Durable Object to pull from its GitHub upstream.
///
/// The DO owns the SQLite that objects land in, so the sync must run there
/// rather than in the Workflow. The actor header is the same trusted signal the
/// auth worker sets; a Workflow in this Worker is inside that trust boundary.
async fn sync_one(env: &Env, repo: &str) -> Result<RepoOutcome> {
    let owner = repo
        .split('/')
        .next()
        .filter(|o| !o.is_empty())
        .ok_or_else(|| Error::RustError(format!("malformed repo path `{}`", repo)))?;

    let headers = Headers::new();
    headers.set("X-Ripgit-Actor-Name", owner)?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post).with_headers(headers);
    let req = Request::new_with_init(
        &format!("https://ripgit.internal/{}/mirror/sync", repo),
        &init,
    )?;

    let stub = env
        .durable_object("REPOSITORY")?
        .id_from_name(repo)?
        .get_stub()?;
    let mut resp = stub.fetch_with_request(req).await?;
    let body = resp.text().await.unwrap_or_default();

    if resp.status_code() >= 400 {
        // Returned as an error so the step retries; the workflow records it if
        // the retries are exhausted.
        return Err(Error::RustError(format!(
            "{} -> HTTP {}: {}",
            repo,
            resp.status_code(),
            body.chars().take(200).collect::<String>()
        )));
    }

    Ok(RepoOutcome {
        repo: repo.to_string(),
        ok: true,
        detail: body.chars().take(200).collect(),
    })
}

// ---------------------------------------------------------------------------
// Scheduled sweep
// ---------------------------------------------------------------------------

/// List every repo enrolled for GitHub pull mirroring.
pub async fn enrolled_repos(env: &Env) -> Result<Vec<String>> {
    let kv = env.kv("REGISTRY")?;
    let mut repos = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut builder = kv.list().prefix(REGISTRY_PREFIX.to_string());
        if let Some(c) = cursor {
            builder = builder.cursor(c);
        }
        let page = builder.execute().await?;

        for key in &page.keys {
            if let Some(repo) = key.name.strip_prefix(REGISTRY_PREFIX) {
                repos.push(repo.to_string());
            }
        }

        match page.cursor {
            Some(c) if !page.list_complete => cursor = Some(c),
            _ => break,
        }
    }

    Ok(repos)
}

/// Start a mirror sweep over every enrolled repo.
///
/// Returns `None` when nothing is enrolled, so a cron tick on an empty
/// deployment does not create an instance that immediately does nothing.
pub async fn start_sweep(env: &Env) -> Result<Option<String>> {
    let repos = enrolled_repos(env).await?;
    if repos.is_empty() {
        return Ok(None);
    }

    let options = WorkflowInstanceCreateOptions::new().params(MirrorInput { repos });
    let instance = env
        .workflow("MIRROR_WORKFLOW")?
        .create_with_options(&options)
        .await?;

    Ok(Some(instance.id()))
}
