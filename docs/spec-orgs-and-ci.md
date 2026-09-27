# Spec: organizations, teams, and CI

Status: draft (2026-09-27; Part 1 revised the same day to use better-auth)
Branch baseline: `artifacts-backend` @ `3895a48`

Two features stand between ripgit and replacing GitHub for a small team:

1. **Orgs and teams** — more than one person can own and write to a repo.
2. **CI** — a push or PR runs the repo's pipeline and reports a result.

Orgs come first. CI needs to know who may trigger a run, who sees its logs,
and whose secrets it may read, and every one of those answers is a role check.

---

## Where we are

**Authentication** lives in the auth worker (`examples/github-oauth`). It turns
a session cookie, agent token, or GitHub OIDC token into trusted
`X-Ripgit-Actor-*` headers, and strips any `X-Ripgit-*` headers the caller sent.

**Authorization** is one string comparison in ripgit:

- `check_write_access` (`src/lib.rs:60`): the actor's display name must equal
  the URL's owner segment.
- `check_read_access` (`src/lib.rs:85`): private repos are readable only by
  that same name match.
- Issues and PRs: the author or repo owner may close; only the owner may merge.
- Scopes (`repo:read`, `repo:write`, `issue:write`, `admin`, ...) are minted by
  the auth worker but only `mirror` is ever checked by ripgit.

**Owner namespace**: `/:owner/` is implicitly a GitHub username. Nothing
records that the name is claimed or by whom. The REGISTRY KV holds
`repo:{owner}/{repo}` → `"public" | "private"`.

**workers-rs fork** (`jlgore/workers-rs` @ `14cc828`, synced with upstream 2026-09-27) adds Artifacts and
Workflows bindings and Rust `#[workflow]` entrypoints. Containers and D1 come
from upstream. Neither feature below is expected to need fork changes; see
[Fork checks](#fork-checks).

---

## Part 1 — Organizations and teams

### Goals

- An owner namespace is either a **user** or an **org**.
- Orgs have members with an org role; teams group members.
- Repos grant roles to teams and to individual users.
- One function answers "what role does this actor have on this repo?", and
  every read, write, issue, PR, settings, and CI path asks it.
- Membership can be synced from a GitHub org, but ripgit's copy is
  authoritative, so a GitHub outage never locks anyone out.

### Non-goals (v1)

- Nested teams, custom roles, SSO/SAML, enterprise-level accounts.
- Renaming users, orgs, or repos. DO names are permanent (`{owner}/{repo}`);
  see AGENTS.md.

### Decision: better-auth (TypeScript) in the auth worker

Identity, sessions, orgs, teams, memberships, invitations and API keys come
from [better-auth](https://www.better-auth.com) running in the auth worker,
which is already TypeScript. It replaces the hand-rolled OAuth, session and
agent-token code in `examples/github-oauth/src/index.ts`.

Why not better-auth-rs (evaluated 2026-09-27 at 1.0.0-alpha.3): it does not
compile for `wasm32-unknown-unknown`. `better-auth-api` hard-depends on
`webauthn-rs` → OpenSSL, `better-auth-core` needs tokio `full` (mio), and every
store trait is `Send + Sync`, which Workers bindings are not. It is also alpha
with no teams yet. It mirrors better-auth's API and wire format, so moving the
auth worker to Rust later stays open.

Plugins used:

| Need | better-auth |
|---|---|
| GitHub sign-in | `socialProviders.github` |
| Orgs, teams, invitations, org roles | `organization({ teams: { enabled: true } })` |
| Agent tokens, git HTTP passwords, org-owned bot keys | `@better-auth/api-key` |
| CLI / agent login without a browser session (later) | device authorization plugin |

Stays custom in the auth worker: the GitHub Actions OIDC exchange for mirror
tokens, and GitHub org sync.

### Division of ownership

One D1 database, bound as `AUTH_DB` in the auth worker and `DIRECTORY` in
ripgit.

- **better-auth owns** its tables (`user`, `session`, `account`,
  `verification`, `organization`, `member`, `invitation`, `team`,
  `teamMember`, `apikey`) and is the only writer. Schema comes from
  `npx @better-auth/cli generate`; the better-auth version is pinned exactly,
  because ripgit reads these tables.
- **ripgit owns** only what is git-specific, prefixed `ripgit_`, and applied
  with `wrangler d1 migrations`:

```sql
-- One row per /:owner/ segment. Claimed once, never reassigned.
CREATE TABLE ripgit_namespaces (
  name        TEXT PRIMARY KEY,           -- lowercase URL segment
  kind        TEXT NOT NULL,              -- 'user' | 'org'
  user_id     TEXT,                       -- kind='user': better-auth user.id
  org_id      TEXT,                       -- kind='org': better-auth organization.id
  created_at  INTEGER NOT NULL
);

-- Grants on a repo, to a team or a single user.
CREATE TABLE ripgit_repo_grants (
  repo          TEXT NOT NULL,            -- "owner/name"
  grantee_kind  TEXT NOT NULL,            -- 'team' | 'user'
  grantee_id    TEXT NOT NULL,            -- team.id | user.id
  role          TEXT NOT NULL,            -- 'read' | 'triage' | 'write' | 'admin'
  PRIMARY KEY (repo, grantee_kind, grantee_id)
);

CREATE TABLE ripgit_org_settings (
  org_id              TEXT PRIMARY KEY,
  default_repo_role   TEXT NOT NULL DEFAULT 'read',  -- 'none' | 'read' | 'write'
  github_org          TEXT                           -- linked GitHub org, if synced
);
```

Repo **visibility** stays in the repo DO config (already built), gaining a
third value: `internal` = readable by any member of the owning org. The OIDC
mirror path currently fails closed on GitHub's `internal`; with orgs it maps to
`internal` directly.

### Identity: authorize on IDs, not names

Today ownership follows the GitHub **username**; a GitHub rename would
silently hand a namespace to whoever next registers the old name.

- The actor is the better-auth `user.id`, sent as `X-Ripgit-Actor-Id`. The
  GitHub numeric ID lives in `account` (`providerId = 'github'`,
  `accountId = '<id>'`), which is how existing owners are matched up.
- A user namespace is claimed on first sign-in (the GitHub login, lowercased)
  if free. An org namespace is claimed when the org is created: an
  `organization` create hook inserts into `ripgit_namespaces` and rejects the
  create if the slug is taken, so users and orgs share one namespace.
- API keys resolve to their owning user (or org, for org-owned keys), then are
  narrowed by the key's permissions and optional repo list (generalizing the
  mirror token's `repoScope`).

### Roles

| Role     | Read code/issues | Open issues/PRs, comment | Close/label others' issues | Push, merge PRs | Settings, grants, visibility, delete |
|----------|:-:|:-:|:-:|:-:|:-:|
| `read`   | ✓ | ✓ | | | |
| `triage` | ✓ | ✓ | ✓ | | |
| `write`  | ✓ | ✓ | ✓ | ✓ | |
| `admin`  | ✓ | ✓ | ✓ | ✓ | ✓ |

Commenting on and opening issues stays open to any signed-in user on a readable
repo, as it is today.

Org roles map onto repos: better-auth `owner` and `admin` → repo `admin` on
every repo in the org; `member` → the org's `default_repo_role`. `member.role`
can hold several comma-separated roles; take the highest.

### Resolving a role

```
effective_role(actor, owner, repo) =
  if namespace(owner).kind == 'user':
      admin  if actor.user_id == namespace.user_id
      else max(direct grants on repo)
  if namespace(owner).kind == 'org':
      admin  if actor's member.role includes owner|admin
      else max(default_repo_role if member,
               grants to teams in teamMember for actor,
               direct user grant)
  then: a public repo floors at 'read' for everyone (including anonymous);
        an internal repo floors at 'read' for org members;
        then narrow by API key permissions and repo list
```

One D1 query, roughly:

```sql
SELECT n.kind, n.user_id, m.role AS org_role, s.default_repo_role,
       (SELECT group_concat(g.role) FROM ripgit_repo_grants g
         WHERE g.repo = ?1
           AND ((g.grantee_kind = 'user' AND g.grantee_id = ?2)
             OR (g.grantee_kind = 'team' AND g.grantee_id IN
                  (SELECT tm.teamId FROM teamMember tm WHERE tm.userId = ?2)))) AS grants
FROM ripgit_namespaces n
LEFT JOIN member m ON m.organizationId = n.org_id AND m.userId = ?2
LEFT JOIN ripgit_org_settings s ON s.org_id = n.org_id
WHERE n.name = ?3;
```

Column names are better-auth's defaults; confirm against the generated schema
for the pinned version.

**Where it runs:** in the ripgit Worker entry (`lib.rs::fetch`), before the DO
is called. The Worker sets an internal header `X-Ripgit-Role` on the request to
the DO. The DO is reachable only from the Worker, and the auth worker already
strips all inbound `X-Ripgit-*`, so the header cannot be forged from outside.
The DO applies the visibility floor (it holds visibility) and returns 404 (not
403) when the result is no access, matching today's private-repo behavior.

Replace `check_write_access` / `check_read_access` / the issue author-or-owner
checks with `require(role, Role::Write)`-style calls that read this header.
Keep one helper so no route re-derives it.

**Latency:** one D1 read per request. Use D1 read replication with the
Sessions API; membership changes may take a moment to reach replicas, which is
acceptable. If it is not, cache per-actor results in the Worker for ~30s.

### Git over HTTP

git sends HTTP Basic auth. The auth worker takes the password as an API key
(username ignored), verifies it with the api-key plugin, and forwards the
owning user. Browser sessions keep using the better-auth session cookie.

### Migration

1. Create the D1 database; run better-auth's schema and ripgit's migrations.
2. Stand up better-auth in the auth worker beside the old code. Existing
   browser sessions end; users sign in again with GitHub (same identity, new
   session).
3. Backfill `ripgit_namespaces` from REGISTRY KV owner segments as
   `kind='user'`. On each owner's first better-auth sign-in, match their
   GitHub ID to the namespace and set `user_id`. Until matched, keep the
   display-name fallback for that namespace only, and log each use.
4. Agent tokens in `OAUTH_KV` keep working during a grace period (old lookup as
   a fallback), while owners reissue them as API keys from the settings page.
   Then remove the KV path.
5. Mirror OIDC exchange: unchanged, but mints a short-lived API key (or keeps
   its KV token) scoped to one repo. Mirror grants move into D1 so org owners
   can manage them.
6. When fallbacks reach zero, delete the old OAuth/session/token code.

### Management surface

better-auth provides the API, not the pages.

- **Auth worker** (owns the data): `/settings` for the user's profile and API
  keys; `/orgs/new`; `/:org/settings/members`, `/teams`, `/invitations`,
  `/github`. HTML plus markdown views, like today's `/settings`.
- **ripgit**: `/:owner/:repo/settings/access` for grants and visibility (its
  own tables), and the owner profile page lists org members and teams read
  from D1.
- Org creation is limited to an allowlist while the instance is small (see
  [Open questions](#open-questions)).

### GitHub sync

Runs in the auth worker, since that is where the membership tables live.

- An org owner links a ripgit org to a GitHub org and maps teams by slug.
- Use a **GitHub App** installed on the GitHub org, not user OAuth tokens:
  installation tokens work on a schedule with no user present and survive the
  linking user leaving.
- Provenance: add a `source` field (`'native' | 'github'`) to `member` and
  `teamMember` via the organization plugin's `additionalFields`. Sync writes and
  removes only `github` rows; someone added in ripgit stays.
- Runs on link, on the auth worker's cron trigger, and optionally on GitHub
  `membership`/`organization` webhooks. A synced GitHub user with no ripgit
  account yet is kept as pending and becomes a member on first sign-in.
- If GitHub is unreachable, sync fails and the last-synced state stays in
  force. Nothing is removed on error.

### Tests

Auth worker: vitest with `@cloudflare/vitest-pool-workers` and a local D1.
ripgit: extend the Miniflare harness (`tests/helpers/mf.mjs`) with the same D1,
seeded with better-auth rows.

- Role matrix: each role × each action, for a user repo and an org repo.
- Visibility: public/private/internal × member/non-member/anonymous, over pages,
  API, and `git clone`.
- 404 for no access, never 403, on private and internal repos.
- Forged `X-Ripgit-Role` from outside is ignored (the auth worker strips it;
  also assert ripgit's Worker overwrites it).
- API key narrowing: a key limited to one repo cannot touch another.
- Namespace collision: an org cannot take a user's name and vice versa.
- Sync never deletes `native` rows; a failed sync changes nothing.
- Migration fallbacks: an unmatched legacy owner and an old KV token still work.

---

## Part 2 — CI

### Goals

- A push or PR runs pipelines defined in the repo as TypeScript.
- Jobs execute in Cloudflare Sandboxes.
- Runs, jobs, steps, logs, and statuses appear on commits, PRs, and an
  Actions page, in HTML and markdown.
- Survives a GitHub outage: nothing here depends on GitHub.

### Non-goals (v1)

- GitHub Actions YAML compatibility.
- Running PRs from forks (there are no forks yet).
- Caches, artifacts upload, and service containers. Design leaves room for
  them.

### Terms

- **Pipeline** — a user's `.ripgit/pipelines/*.ts` file. (Not "workflow",
  to avoid confusion with Cloudflare Workflows, which run them.)
- **Run** — one pipeline triggered by one event at one commit.
- **Job** — a unit that gets its own sandbox. **Step** — one command or TS
  function inside a job.

### Components

```
                   push / PR event
 ripgit (Rust) ─────────────────────────────┐
   Repository DO:                           │ Workflow binding
     ci_runs / ci_jobs / ci_steps tables    │ (script_name = ripgit-ci)
     trigger matching (cached plans)        ▼
   GET  /:o/:r/archive/:sha          ripgit-ci (TypeScript Worker)
   POST /:o/:r/ci/report  ◄──────────  CiRunWorkflow (Cloudflare Workflow)
          ▲   service binding             plan step → job steps (fan-out)
          │                              Sandbox DO (@cloudflare/sandbox)
          └──── logs → R2 CI_LOGS ◄────── exec stream per step
```

- **ripgit** (Rust) stays the system of record: run state lives in the repo DO
  next to the commits it describes.
- **ripgit-ci** is a new TypeScript Worker in `ci/`, because the Sandbox SDK
  is TS. It owns `CiRunWorkflow` and the Sandbox DO class and container image.
- The two talk over service bindings in both directions. Sandboxes get **no
  credentials**: source is written into the sandbox by the Workflow, and
  results are reported by the Workflow, never by code in the container.

### Pipeline definition

```ts
// .ripgit/pipelines/test.ts
import { pipeline } from "@ripgit/ci";

export default pipeline({
  name: "test",
  on: {
    push: { branches: ["main"] },
    pull_request: { branches: ["main"] },
  },
  jobs: {
    test: {
      image: "default",          // sandbox image profile, see below
      timeout: "20 minutes",
      steps: [
        { name: "build", run: "cargo build --locked" },
        { name: "test", run: "cargo test --locked" },
      ],
    },
    lint: {
      steps: [
        {
          name: "check todo count",
          // TS steps run inside the job's sandbox, not in the Worker.
          fn: async ({ sh }) => {
            const out = await sh("grep -rc TODO src | wc -l");
            if (Number(out.stdout) > 100) throw new Error("too many TODOs");
          },
        },
      ],
    },
  },
});
```

- `@ripgit/ci` is a small package (in `ci/sdk/`) pre-installed in the image.
- `on` must be plain data so triggers can be matched without booting a
  sandbox. `jobs` may be computed (loops, matrices) at plan time.
- `fn` steps are located by `(pipeline file, job, step name)` and executed by
  re-importing the module inside the job's sandbox.

### Run lifecycle

1. **Event.** After a successful receive-pack, PR open/update, or manual
   re-run, the repo DO looks for `.ripgit/pipelines/*.ts` at the new commit.
2. **Match.** Plans are cached in the DO keyed by the pipeline file's blob
   hash. With a cached plan, the DO matches `on` against the event in Rust and
   creates runs only for matches. With no cached plan (file changed), it
   creates a run in state `planning` and lets the Workflow decide.
3. **Start.** The DO records the run (`queued`) and creates a `CiRunWorkflow`
   instance with id `{owner}/{repo}/{run_id}` and params
   `{owner, repo, run_id, sha, event, pipeline_path}`. Creating the instance
   from inside the DO: via the Workflow binding with `script_name`, or via a
   service binding to ripgit-ci (see [Fork checks](#fork-checks)).
4. **Plan step** (durable): start a plan sandbox, write the archive, run
   `ripgit-ci plan <file>` (bun evaluates the TS, prints the plan JSON). Report
   the plan to ripgit, which caches it and may cancel the run if `on` does not
   match.
5. **Job steps**: one Workflow step per job, run in parallel up to the
   concurrency limit. Each gets a sandbox keyed by `{run_id}/{job}`, writes the
   archive, then runs each step with streamed output. Step status and log
   chunks are reported as they happen.
6. **Finish.** The run's final status is written to the DO; the commit and PR
   pages show it.

Workflow step names must stay stable across deploys (they are replay keys):
`plan`, `job:{name}`, `report:{name}`.

### Source checkout

New endpoint on ripgit: `GET /:owner/:repo/archive/:sha` returning a tar
stream of the tree at that commit, built from `trees` + `read_blob`. ripgit-ci
calls it over the service binding and streams it into the sandbox. No git, no
token, no network egress needed for checkout.

Pipelines that need history (`git describe`, changelogs) get a
`checkout: { history: true }` option in a later phase, served as a pack over
the same binding.

### Storage

In the repo DO (`src/schema.rs`):

```sql
CREATE TABLE ci_runs (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  number       INTEGER NOT NULL,          -- per-repo sequence, like issues
  pipeline     TEXT NOT NULL,             -- ".ripgit/pipelines/test.ts"
  event        TEXT NOT NULL,             -- 'push' | 'pull_request' | 'manual'
  ref          TEXT NOT NULL,
  sha          TEXT NOT NULL,
  pr_number    INTEGER,
  status       TEXT NOT NULL,             -- planning|queued|running|success|failure|cancelled|error
  actor_id     TEXT NOT NULL,
  workflow_id  TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  finished_at  INTEGER
);
CREATE TABLE ci_jobs  (id, run_id, name, status, started_at, finished_at);
CREATE TABLE ci_steps (id, job_id, idx, name, status, exit_code,
                       log_key, started_at, finished_at);
CREATE TABLE ci_plans (blob_hash TEXT PRIMARY KEY, plan_json TEXT NOT NULL);
```

Logs go to R2 bucket `CI_LOGS` at `{owner}/{repo}/{run}/{job}/{step}.log`,
written by ripgit-ci, since logs can exceed the DO row limit and are served
directly. Retention: 90 days by R2 lifecycle rule.

Mind the 100-parameter limit and avoid `NULL` parameter bindings (AGENTS.md)
in the new inserts.

### Permissions (depends on Part 1)

| Action | Required role |
|---|---|
| Trigger by push / PR from a branch in the repo | `write` (already required to push) |
| See runs and logs | `read` on the repo |
| Re-run, cancel | `write` |
| Manage repo secrets | `admin`; org secrets: org owner |

### Secrets (phase 4)

- Stored encrypted in `DIRECTORY` (org and repo scope), with the key in
  Cloudflare Secrets Store.
- Injected by ripgit-ci as env vars on `exec` for jobs that declare them
  (`secrets: ["NPM_TOKEN"]`). Never passed to the plan step.
- Masked in logs by exact-match replacement before log chunks are written.

### Images and limits

- One `default` image to start: Debian + git + bun + node + rustup (with
  `wasm32-unknown-unknown`, so ripgit can build itself) + build-essential.
- More profiles later as Sandbox instance types / images, keyed by `image:`.
- Defaults: job timeout 30 min; 4 concurrent jobs per owner namespace; plan
  step timeout 60 s. Stored in `org_settings` once Part 1 lands.

### UI

- `/:owner/:repo/actions` — run list (filter by pipeline, branch, status).
- `/:owner/:repo/actions/:n` — run detail, jobs, per-step logs from R2.
- Status dot on commit rows, commit page, and PR page; PR page lists checks.
- Markdown renderers for all of the above (agents read CI results too).

### Tests

- Rust unit: trigger matching against cached plans; archive endpoint output
  equals `git archive` for the workers-rs fixture.
- ripgit-ci: Workflow tested with vitest-pool-workers + a stubbed sandbox;
  plan evaluation of sample pipelines.
- e2e (Miniflare): push fixture with a pipeline → run recorded → report calls
  update status → page shows it. Real sandbox execution tested only against a
  deployed preview.

---

## Fork checks

Verify before relying on them; none are expected to need new fork code.

1. **Cross-script Workflow binding from Rust.** `[[workflows]]` with
   `script_name = "ripgit-ci"` in ripgit's wrangler.toml, called via
   `env.workflow("CI_WORKFLOW")?.create_with_options(...)`. If it fails, fall
   back to a service binding (`env.service("CI")`) and let ripgit-ci create the
   instance.
2. **Workflow binding inside a DO.** The DO's `Env` must expose the binding;
   the MirrorWorkflow path suggests it does, but it is currently invoked from
   the Worker, not the DO.
3. **D1 from Rust** (upstream `worker::D1Database`): confirm the Sessions API
   for read replicas is exposed; if not, that is the one likely fork addition.
4. **Streaming a tar response** from the DO without buffering the whole
   archive (128 MB limit).

---

## Phases

| Phase | Scope | Done when |
|---|---|---|
| 0 | Shared D1; better-auth in the auth worker (GitHub sign-in, sessions, API keys) beside the old code; `ripgit_namespaces` backfill; `effective_role` + `X-Ripgit-Role` behind legacy fallbacks | All existing tests pass with role checks; sign-in and git push work through better-auth; fallback count is zero |
| 1 | Organization plugin with teams, namespace hook, `ripgit_repo_grants`, `internal` visibility, org settings pages (auth worker) and access page (ripgit); remove old auth code | Role and visibility matrix tests pass; an org repo is usable by two people |
| 2 | GitHub App sync in the auth worker | A GitHub org's teams show up in ripgit and stay in place when sync fails |
| 3 | CI MVP: ripgit-ci worker, archive endpoint, push trigger, shell steps, logs, Actions page | ripgit's own pipeline builds and tests ripgit on push |
| 4 | CI: PR triggers + PR checks, TS `fn` steps, matrix, secrets, cancel/re-run | A PR shows passing checks before merge |
| 5 | Required checks + branch protection (also closes the force-push gap in TODOS.md) | Merge is blocked on a failing required check |

Phases 0–2 and 3 can overlap once Phase 0 lands: CI MVP only needs
`effective_role`, not full org management.

---

## Open questions

1. **Org creation policy** — open to any signed-in user, or instance-admin
   allowlist? (Recommended: allowlist while the instance is private.)
2. **Name collisions at backfill** — a GitHub org login already used as a
   ripgit user namespace. Proposed: first claim wins, the other is renamed
   manually.
3. **Sandbox cost ceiling** — a per-owner monthly minute cap, or just
   concurrency limits for now?
4. **Pipeline naming** — `.ripgit/pipelines/` vs `.ripgit/workflows/`.
5. **Mirrored repos** — should mirrors run CI? Proposed: off by default;
   GitHub already ran it, and a promoted mirror turns it on.
6. **Agent token migration** — reissue as API keys during a grace period
   (proposed), or import existing KV tokens into the `apikey` table so nothing
   breaks? Importing depends on better-auth's key hashing format.
7. **Auth worker location** — it is still `examples/github-oauth`. With
   better-auth it becomes a required part of ripgit; move it to `auth/`?
