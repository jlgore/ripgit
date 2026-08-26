# GitHub OAuth Auth Worker

This example Worker sits in front of ripgit, handles GitHub OAuth, issues browser session cookies, mints long-lived tokens, and forwards trusted `X-Ripgit-Actor-*` headers to the main ripgit Worker through a Service Binding.

## What It Does

- `GET /` - landing page for browsers plus text mode for curl/agents
- `GET /settings` - token management page after sign-in plus text mode for curl/agents
- `GET /login` / `GET /logout` - browser login/logout flow
- `GET /oauth/authorize` / `POST /oauth/token` - OAuth provider flow for programmatic clients
- `POST /oidc/github/exchange` - trade a GitHub Actions OIDC token for a short-lived, repo-scoped push token
- `POST /settings/tokens` - create a long-lived token
- `POST /settings/tokens/:id/revoke` - revoke a long-lived token

Everything else is forwarded to ripgit.

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

ripgit's own ownership check is owner-wide: an actor named `jlgore` may write to
any repo under `/jlgore/`. Mirror tokens carry a `repoScope`, and the auth worker
rejects any proxied request whose path does not match it, so a workflow in one
repo cannot push to a sibling.

## Required Bindings And Secrets

Set these in `wrangler.toml` or as Worker secrets:

- `GITHUB_CLIENT_ID` - GitHub OAuth App client ID (`[vars]`)
- `GITHUB_CLIENT_SECRET` - GitHub OAuth App client secret (`wrangler secret put GITHUB_CLIENT_SECRET`)
- `SESSION_SECRET` - random 32+ character secret for signing browser sessions (`wrangler secret put SESSION_SECRET`)
- `OIDC_AUDIENCE` - this deployment's URL, required for GitHub Actions mirroring (`[vars]`)
- `OAUTH_KV` - KV namespace used for OAuth state, issued tokens, and token indexes
- `RIPGIT` - Service Binding that points at the main ripgit Worker

`workers-oauth-provider` also injects the `OAUTH_PROVIDER` helper at runtime.

## GitHub OAuth App Setup

Create a GitHub OAuth App at <https://github.com/settings/applications/new>.

- Homepage URL: your deployed auth worker URL, for example `https://git-auth.example.workers.dev`
- Authorization callback URL: `https://git-auth.example.workers.dev/oauth/callback`
- Local dev callback URL: `http://localhost:8787/oauth/callback`

## Local Development

From the repo root:

```bash
cd examples/github-oauth
npm install
npm run dev:full
```

That runs:

- the auth worker on `http://localhost:8787`
- the main ripgit Worker through the local Service Binding declared in `wrangler.toml`

Then:

1. Visit `http://localhost:8787`
2. Sign in with GitHub
3. Open `http://localhost:8787/settings`
4. Generate a token
5. Push a repo with that token

Example push:

```bash
git remote add origin http://USERNAME:TOKEN@localhost:8787/USERNAME/my-project
git push origin main
```

## Deployment

Create the KV namespace and fill the IDs into `examples/github-oauth/wrangler.toml`:

```bash
wrangler kv namespace create OAUTH_KV
wrangler kv namespace create OAUTH_KV --preview
```

Set the secrets:

```bash
wrangler secret put GITHUB_CLIENT_SECRET
wrangler secret put SESSION_SECRET
```

Deploy ripgit first, then the auth worker:

```bash
wrangler deploy
cd examples/github-oauth
wrangler deploy
```

Make sure the `[[services]]` binding in `examples/github-oauth/wrangler.toml` points at the deployed ripgit Worker name.

After deployment, update the GitHub OAuth App callback URL to your deployed auth worker URL.

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
