/**
 * @ripgit/ci — define a ripgit pipeline.
 *
 * A pipeline is a `.ripgit/pipelines/*.ts` file whose default export is a
 * `pipeline({...})`. It is evaluated with bun inside a sandbox to produce a
 * plan; each job then runs in its own fresh sandbox, steps in order, each step
 * a bash script run from the repository root. A failing step fails its job and
 * skips the job's remaining steps.
 *
 *   import { pipeline } from "@ripgit/ci";
 *
 *   export default pipeline({
 *     name: "test",
 *     on: { push: { branches: ["main", "release/*"] } },
 *     jobs: {
 *       test: {
 *         timeout: "20 minutes",
 *         steps: [
 *           { name: "build", run: "cargo build --locked" },
 *           { name: "test", run: "cargo test --locked" },
 *         ],
 *       },
 *     },
 *   });
 */

export interface Step {
  /** Shown in the run page. */
  name: string;
  /** A bash script, run with `set -eo pipefail` from the repository root. */
  run: string;
}

export interface Job {
  /** Wall-clock limit, e.g. "20 minutes", "90s", "1h". Default 30 minutes. */
  timeout?: string;
  /** Non-secret environment variables for every step. */
  env?: Record<string, string>;
  steps: Step[];
}

export interface Triggers {
  /** Run on pushes. Omit `branches` to run for every branch; `*` matches any run of characters. */
  push?: { branches?: string[] };
}

export interface Pipeline {
  name: string;
  on: Triggers;
  /** Job names become part of URLs: letters, digits, `.`, `_`, `-`. */
  jobs: Record<string, Job>;
}

/** Identity function that gives a pipeline file type checking. */
export function pipeline(definition: Pipeline): Pipeline {
  return definition;
}
