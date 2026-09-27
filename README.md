# ripgit

A self-hostable git remote backed by Cloudflare Durable Objects. One DO per repo, SQLite storage, FTS5 search, delta compression via [xpatch](https://github.com/ImGajeed76/xpatch). Built in Rust with [workers-rs](https://github.com/cloudflare/workers-rs).

**Live example: [git.theagents.company](https://git.theagents.company/) — [deathbyknowledge's repos](https://git.theagents.company/deathbyknowledge/) including `agents` (~1k commits) and `curl` (~40k commits)**

```bash
git remote add origin https://your-worker.workers.dev/username/myproject
git push origin main
```

## Features

- **Standard git remote** — `git push`, `git clone`, `git fetch` with any git client
- **CI** — `.ripgit/pipelines/*.ts` run on push in Cloudflare Sandboxes via the `ripgit-ci` Worker (`ci/`); results under each repo's Actions tab
- **Auth via Service Binding** — sits behind the auth worker in `auth/` (better-auth: GitHub sign-in, orgs, teams, API keys); roles per repo, public repos readable by anyone
- **Agent-first UI** — browsable pages also negotiate `text/markdown` and `text/plain`, with explicit actions and curl-friendly paths
- **Web UI** — file browser, commit history, diffs, code search, syntax highlighting, branch selector, markdown README, repo settings
- **Full-text search** — FTS5 over file content and commit messages. Supports `@author:`, `@message:`, `@path:`, `@ext:`, `@content:` query prefixes
- **Raw file serving** — `/:owner/:repo/raw/:ref/*path`
- **Read API** — refs, commits, trees, files, diffs, search, stats
- **Repo registry** — repos listed on the owner profile page after first push
- **Delta compression** — 5–20x compression on real repos depending on file churn
- **One DO per repo** — strict isolation, scales horizontally

## URLs

```
/:owner/                   owner profile — lists your repos
/:owner/:repo/             repo home — file tree, README, recent commits
/:owner/:repo/commits      commit history
/:owner/:repo/commit/:sha  commit detail with diff
/:owner/:repo/tree/:ref/*  directory browser
/:owner/:repo/blob/:ref/*  file viewer
/:owner/:repo/raw/:ref/*   raw file bytes
/:owner/:repo/search-ui    full-text search
/:owner/:repo/settings     stats, index rebuilds, config, delete (owner only)
```

## API

All endpoints under `/:owner/:repo/`.

| Endpoint | Description |
|---|---|
| `GET /refs` | List branches and tags |
| `GET /log?ref=main&limit=50` | Commit history |
| `GET /commit/:hash` | Single commit |
| `GET /tree/:hash` | Directory listing |
| `GET /blob/:hash` | File content |
| `GET /file?ref=main&path=src/lib.rs` | File at ref + path |
| `GET /search?q=TODO` | Code search |
| `GET /search?q=fix&scope=commits` | Commit message search |
| `GET /diff/:sha` | Commit diff |
| `GET /compare/base...head` | Two-commit comparison |
| `GET /stats` | Compression and storage stats |

## Text Mode For Agents

Browsable pages support negotiated text mode for curl, scripts, and agents.

```bash
curl -H 'Accept: text/markdown' https://your-worker.workers.dev/alice/repo/
curl -H 'Accept: text/plain' https://your-worker.workers.dev/alice/repo/commits
curl 'https://your-worker.workers.dev/alice/repo/tree/main/src?format=md'
```

- `Accept: text/markdown` returns a markdown view
- `Accept: text/plain` returns the same content as plain text
- `?format=md` or `?format=text` works when you can't keep headers attached while following links
- Text pages list bare GET paths under headings like `Files (GET paths)` and spell out POST actions, fields, and requirements in an `Actions` section

## Authentication

ripgit reads identity from trusted `X-Ripgit-Actor-*` headers, which are only settable by an upstream auth worker via Service Binding (not from the public internet).

- **Anonymous** — read access to all repos
- **Authenticated** — read + write to repos under your username

### GitHub OAuth example

`auth/` is a TypeScript Cloudflare Worker built on better-auth: GitHub sign-in, browser sessions, organizations and teams, and API keys for agents/scripts. It forwards requests to ripgit via Service Binding; ripgit resolves each caller's role from the shared D1 database. Its landing page and `/settings` also support the same text-mode negotiation for curl-driven agents.

See `auth/README.md` for a focused deploy/setup guide.

**Local dev:**

```bash
cd auth
npm install
npm run dev:full   # auth worker on :8787, ripgit as service binding
```

Visit `http://localhost:8787` → sign in → go to `/settings` → create a token → push:

```bash
git remote add origin http://username:TOKEN@localhost:8787/username/myrepo
git push origin main
```

**First-time setup:**

1. Create a GitHub OAuth App — callback URL: `http://localhost:8787/oauth/callback`
2. Set `GITHUB_CLIENT_ID` in `auth/wrangler.toml`
3. `wrangler secret put GITHUB_CLIENT_SECRET`
4. `wrangler secret put SESSION_SECRET`
5. `wrangler kv namespace create OAUTH_KV` → fill IDs into `wrangler.toml`

**Deploy:**

```bash
wrangler deploy                          # ripgit worker
cd auth && wrangler deploy   # auth worker
```

Update the GitHub OAuth App's callback URL to your deployed auth worker URL.

### Push test script

```bash
./scripts/push-test.sh -u username -t TOKEN -w https://your-worker.dev -r /path/to/repo
```

## Setup (ripgit only, no auth)

Prerequisites: Rust, [wrangler](https://developers.cloudflare.com/workers/wrangler/), LLVM (for zstd-sys).

```bash
brew install llvm
git clone https://github.com/your-org/ripgit
cd ripgit
wrangler kv namespace create REGISTRY   # fill ID into wrangler.toml
wrangler deploy
```

Without the auth worker in front, all repos are publicly readable and writable by anyone with the URL.

## Architecture

```
browser / git client / agent
  │
  ▼
Auth Worker  (auth — required for writes)
  │  validates session/token, sets X-Ripgit-Actor-* headers
  │  Service Binding
  ▼
ripgit Worker  (entry, routing)
  │  /:owner/:repo/* → DO named "{owner}/{repo}"
  │  /:owner/        → profile page (queries REGISTRY KV)
  ▼
Repository Durable Object  (one per repo)
  ├── schema.rs   11 tables + 3 FTS5 virtual tables
  ├── pack.rs     streaming pack parser + pack generator
  ├── git.rs      smart HTTP protocol (receive-pack, upload-pack)
  ├── store.rs    delta compression, commit graph, FTS rebuild
  ├── api.rs      read API
  ├── diff.rs     tree diff + line-level diffs
  └── web.rs      server-rendered HTML (9 pages)
  ▼
SQLite  (up to 10 GB per DO)

KV namespaces:
  REGISTRY  — "repo:{owner}/{repo}" written on first push
  OAUTH_KV  — tokens, sessions (auth worker only)
```

## Pushing large repos

Cloudflare Workers has a 100 MB request body limit. Push large repos incrementally:

```bash
./scripts/push-test.sh -u username -t TOKEN -r /path/to/repo -s 200
```

Or manually in checkpoints:

```bash
STEP=250
for FP in $(seq $STEP $STEP $(git rev-list --first-parent --count main)); do
  SHA=$(git rev-list --reverse --first-parent main | sed -n "${FP}p")
  git push origin "${SHA}:refs/heads/main"
done
git push origin main
```

## Known limitations

- **DO storage timeout** — pushes with >~10K objects per push can exceed the 30 s timeout; push incrementally
- **100 MB request body limit** — hard Workers platform constraint
- **No force push** — may produce inconsistent state
- **No annotated tags** — silently dropped; lightweight tags work

## License

AGPL-3.0. See [LICENSE](LICENSE).

## Acknowledgments

Inspired by [pgit](https://github.com/ImGajeed76/pgit) and the [xpatch](https://github.com/ImGajeed76/xpatch) delta compression library.
