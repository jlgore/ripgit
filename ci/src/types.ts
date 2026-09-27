import type { Sandbox } from "@cloudflare/sandbox";

export interface Env {
  Sandbox: DurableObjectNamespace<Sandbox>;
  CI_RUN: Workflow<RunParams>;
  /** Service binding to ripgit: source archives in, status reports out. */
  RIPGIT: Fetcher;
  /** Step logs, read back by ripgit's Actions pages. */
  CI_LOGS: R2Bucket;
}

/** What ripgit sends to start a run (see src/ci.rs, StartRun). */
export interface RunParams {
  owner: string;
  repo: string;
  run: number;
  sha: string;
  ref: string;
  event: "push";
  pipeline: string;
}
