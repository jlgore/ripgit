# ripgit-ci

Runs ripgit pipelines on Cloudflare Workflows and Sandboxes.

A pipeline is a `.ripgit/pipelines/*.ts` file in a repository. When a push moves
a branch, ripgit starts one run per pipeline file at the new commit. Each run is
a `CiRunWorkflow` instance:

1. **plan** — a sandbox gets the source (from ripgit, as a tar) and evaluates
   the pipeline with bun. The file is the repository's own code, so it runs in
   the sandbox, never in the Worker. The result is the job list and whether
   the push matches the pipeline's triggers.
2. **job:\<name\>** — each job runs in its own fresh sandbox, in parallel: the
   source is checked out, then each step runs as a bash script from the repo
   root (`set -eo pipefail`, stderr merged into stdout) under the job's
   timeout. A failing step skips the rest of its job. Step logs go to R2.
3. **report:run** — the final status.

Progress is reported to ripgit as it happens and shows up under the repo's
**Actions** tab.

## Writing a pipeline

```ts
// .ripgit/pipelines/test.ts
import { pipeline } from "@ripgit/ci";

export default pipeline({
  name: "test",
  on: { push: { branches: ["main", "release/*"] } }, // omit branches for all
  jobs: {
    test: {
      timeout: "20 minutes",             // default 30 minutes, max 6 hours
      env: { RUST_BACKTRACE: "1" },      // non-secret only
      steps: [
        { name: "build", run: "cargo build --locked" },
        { name: "test", run: "cargo test --locked" },
      ],
    },
  },
});
```

`@ripgit/ci` is baked into the runner image and linked in during planning; the
repo does not install it. Steps also see `CI=true`, `RIPGIT_CI=true`,
`RIPGIT_SHA`, and `RIPGIT_REF`.

The runner image (`Dockerfile`) is the Sandbox base plus build-essential,
clang/llvm, and Rust with the `wasm32-unknown-unknown` target. Its base tag must
match the `@cloudflare/sandbox` version in `package.json` exactly.

## How it talks to ripgit

Only through service bindings, in both directions; sandboxes hold no
credentials.

| Direction | Call |
|---|---|
| ripgit → ripgit-ci | `POST /runs` `{owner, repo, run, sha, ref, event, pipeline}` |
| ripgit-ci → ripgit | `GET /:owner/:repo/archive/:sha` (tar) |
| ripgit-ci → ripgit | `POST /:owner/:repo/ci/report` (`plan` / `job` / `step` / `run`) |

ripgit-ci calls ripgit as the `ci` actor scoped to the run's repo
(`X-Ripgit-Actor-Kind: ci`, `X-Ripgit-Actor-Repo`). ripgit is not publicly
routable, and the auth worker strips `X-Ripgit-*` from public traffic, so only
bound Workers can present that identity.

## Deploy

```bash
wrangler r2 bucket create ripgit-ci-logs   # shared with ripgit
cd ci && npm ci && npx wrangler deploy     # builds and pushes the image
```

Then deploy ripgit, whose `CI` service binding and `CI_LOGS` bucket point here.
Without ripgit-ci deployed, ripgit simply starts no runs.

## Local development

Docker is required for sandboxes. From the repo root, all three workers
together:

```bash
ci/node_modules/.bin/wrangler dev -c auth/wrangler.toml -c wrangler.toml -c ci/wrangler.jsonc
```

## Tests

```bash
npm test          # plan validation and trigger matching (sdk/plan-core.ts)
npm run typecheck
```

ripgit's side (runs, archives, reports, pages) is covered by `tests/ci.spec.mjs`
at the repo root, against a stub of this Worker.

## Not yet

Pull request triggers, TypeScript function steps, matrices, secrets,
cancel/re-run, and required checks are later phases in
`docs/spec-orgs-and-ci.md`.
