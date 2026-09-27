# ripgit Auth Worker

Sits in front of ripgit and answers "who is this?". Identity comes from
[better-auth](https://www.better-auth.com) (`src/auth.ts`): GitHub sign-in,
browser sessions, organizations and teams, and API keys, all stored in the
`ripgit-directory` D1 database. The worker forwards each request to ripgit
through a Service Binding with trusted `X-Ripgit-Actor-*` headers; ripgit reads
the same database to decide what that actor may do (`src/authz.rs`).

## What It Does

- `* /api/auth/*` - better-auth: OAuth callback, sessions, organizations, teams, API keys
- `GET /` - landing page for browsers plus text mode for curl/agents
- `GET /login` / `GET /logout` - browser sign-in/sign-out (`?next=` a same-origin path)
- `GET /settings` - API key management after sign-in plus text mode for curl/agents
- `POST /settings/tokens` - create an API key (shown once)
- `POST /settings/tokens/:id/revoke` - revoke an API key
- `POST /oidc/github/exchange` - trade a GitHub Actions OIDC token for a short-lived, repo-scoped push token
- `GET /orgs`, `/orgs/:slug` - organizations: create one, add members by GitHub login, manage teams, set the default repo role (see `src/orgs.ts` for every route)

Everything else is forwarded to ripgit.

On first sign-in a user claims the owner namespace matching their GitHub login
(`ripgit_namespaces`). Organizations claim their slug when created, from the
same pool, so a user and an org can never share a name, and a slug can never
change. Only logins listed in `ORG_CREATORS` may create organizations.

Org members get the org's default repo role (`read` unless changed) on every
org repo; owners and admins get `admin`. Grant a team or a person more on one
repo from that repo's settings page. Repos can be `public`, `private`, or (for
org repos) `internal`: readable by org members only.

API keys are sent as `Authorization: Bearer KEY` or as the password of an
HTTPS git remote (the username is ignored).

## Mirroring From GitHub Without A Long-Lived Secret

A GitHub Actions workflow can push a mirror into ripgit using an OIDC token that
Actions signs at runtime. Nothing long-lived is stored in GitHub secrets, and a
minted token lives ten minutes and may write to exactly one repo.

```
Actions (id-token: write) --OIDC JWT--> POST /oidc/github/exchange
                                          verify signature against GitHub JWKS
                                          check iss / aud / exp / jti
                                          look up the mirror allowlist
                                        <--short-lived token-- git push
```

`POST /oidc/github/exchange` takes `{"subject_token": "<JWT>"}` and returns
`{"access_token", "expires_in", "target"}`. Use the token as a bearer header or
as the password of an HTTPS git remote.

A copy-paste workflow lives in `examples/github-actions-mirror/mirror.yml`.

### Enrolling

Enrollment has two levels. Owner-level trust says any repo under an owner may
mirror to the matching path in ripgit:

```bash
wrangler kv key put --binding OAUTH_KV "mirror-owner:jlgore" \
  '{"refs":["refs/heads/main"]}'
```

Adding `mirror.yml` to a repo is then the only step needed to start mirroring
it. This stays safe because the minted token is scoped to the single repository
the signed OIDC claim names, and the ripgit target is derived from that claim
rather than taken from the request -- a workflow can only ever write to its own
mirror.

A per-repo entry overrides the owner rule, for a target that does not match the
GitHub path or a different set of refs:

```bash
wrangler kv key put --binding OAUTH_KV "mirror:jlgore/ripgit" \
  '{"target":"jlgore/ripgit","refs":["refs/heads/main"]}'
```

- `target` — the `owner/repo` path in ripgit this workflow may push to.
- `refs` — optional; if set, only these refs may trigger an exchange.

Keys are lowercased on lookup. Remove a key to revoke; already minted tokens
still expire on their own within ten minutes.

### Why The Audience Must Be Pinned

`OIDC_AUDIENCE` must be set to this deployment's URL, and the workflow must
request that same audience. GitHub's default audience is the repository owner's
URL, which is shared with every other service that owner runs — without pinning,
a token minted for any of them could be replayed here. The exchange endpoint
refuses to run at all when `OIDC_AUDIENCE` is unset rather than falling back.

### Scope Enforcement

A mirror token carries a `repoScope`. The auth worker rejects any proxied
request whose path does not match it, and ripgit grants a mirror agent write
access to that one repo only, so a workflow in one repo cannot push to a
sibling.

## Required Bindings And Secrets

- `AUTH_DB` - the `ripgit-directory` D1 database. ripgit binds the same database as `DIRECTORY`.
- `OAUTH_KV` - KV for the GitHub OIDC key cache, mirror enrollment, and short-lived mirror tokens
- `RIPGIT` - Service Binding that points at the main ripgit Worker
- `BETTER_AUTH_URL` - this worker's public origin (`[vars]`)
- `GITHUB_CLIENT_ID` - the GitHub App's client ID, from the Secrets Store (`ripgit_github_app_client_id`)
- `ORG_CREATORS` - comma-separated GitHub logins allowed to create organizations (`[vars]`)
- `OIDC_AUDIENCE` - this deployment's URL, required for GitHub Actions mirroring (`[vars]`)
- `GITHUB_CLIENT_SECRET` - the GitHub App's client secret, from the Secrets Store (`ripgit_github_client_secret`)
- `BETTER_AUTH_SECRET` - 32+ random bytes: `openssl rand -base64 32 | wrangler secret put BETTER_AUTH_SECRET` (or add it to the Secrets Store and bind it like the two above)

Each secret may be a Secrets Store binding or a plain string (a Worker secret or
`.dev.vars`); `readSecret` in `src/auth.ts` accepts either. A missing one fails
requests with a message naming it, and is picked up without a redeploy once
added.

## Database

`migrations/0001_better_auth.sql` is generated from `src/auth.ts` by
better-auth's own migration planner; `0002_ripgit.sql` holds ripgit's tables.
better-auth is pinned to an exact version because ripgit reads its tables.
After changing `src/auth.ts` or bumping better-auth:

```bash
npm run schema:generate    # prints the full schema; diff it into a new migration
```

## GitHub App Setup

Sign-in uses the ripgit GitHub App (the same App org sync will use). In the
App's settings, under "Identifying and authorizing users":

- Callback URL: `https://git-auth.example.workers.dev/api/auth/callback/github`
- Local dev callback URL: `http://localhost:8787/api/auth/callback/github`
- Leave "Request user authorization (OAuth) during installation" off.

## Local Development

```bash
cd auth
npm install
cp .dev.vars.example .dev.vars          # set BETTER_AUTH_SECRET; seed the local store as it says
npx wrangler d1 migrations apply ripgit-directory --local
npm run dev:full
```

That runs the auth worker on `http://localhost:8787` with ripgit behind it
through the local Service Binding. Then sign in at `http://localhost:8787`,
create an API key at `/settings`, and push:

```bash
git remote add origin http://USERNAME:KEY@localhost:8787/USERNAME/my-project
git push origin main
```

## Deployment

Create the database, put its ID into both `auth/wrangler.toml` (`AUTH_DB`) and
the root `wrangler.toml` (`DIRECTORY`), and apply the migrations:

```bash
wrangler d1 create ripgit-directory
cd auth
wrangler d1 migrations apply ripgit-directory --remote
```

The GitHub App's client ID and secret come from the Secrets Store. Set the
remaining secret, then deploy ripgit first and the auth worker second:

```bash
openssl rand -base64 32 | wrangler secret put BETTER_AUTH_SECRET
cd .. && wrangler deploy
cd auth && wrangler deploy
```

Make sure the `[[services]]` binding in `auth/wrangler.toml` points at the
deployed ripgit Worker name, and that the GitHub OAuth App's callback URL is
`{BETTER_AUTH_URL}/api/auth/callback/github`.

## Text Mode

The auth worker landing page and `/settings` support the same text-mode negotiation as ripgit repo pages:

```bash
curl -H 'Accept: text/markdown' https://your-auth-worker.example/
curl -H 'Accept: text/plain' https://your-auth-worker.example/settings
curl 'https://your-auth-worker.example/settings?format=md'
```

- `Accept: text/markdown` returns markdown
- `Accept: text/plain` returns plain text
- `?format=md` and `?format=text` work when you can't keep headers attached while following links

The text pages explain what the auth worker does, which paths are available, and which POST actions require an authenticated session.
