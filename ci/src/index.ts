/**
 * ripgit-ci: runs ripgit pipelines in Cloudflare Sandboxes.
 *
 * Reached only through ripgit's CI service binding (no public route):
 *   POST /runs   start a run; body is RunParams (see src/ci.rs, StartRun)
 *
 * Each run is one CiRunWorkflow instance, which fetches source from ripgit
 * and reports status back to it over the RIPGIT binding.
 */

import type { Env, RunParams } from "./types";

export { Sandbox } from "@cloudflare/sandbox";
export { CiRunWorkflow } from "./workflow";

function isRunParams(body: unknown): body is RunParams {
  const b = body as Partial<RunParams>;
  return (
    typeof b === "object" &&
    b !== null &&
    typeof b.owner === "string" &&
    typeof b.repo === "string" &&
    Number.isInteger(b.run) &&
    typeof b.sha === "string" &&
    /^[0-9a-f]{40}$/.test(b.sha) &&
    typeof b.ref === "string" &&
    b.event === "push" &&
    typeof b.pipeline === "string" &&
    b.pipeline.startsWith(".ripgit/pipelines/")
  );
}

/** Workflow instance ids allow [A-Za-z0-9_-]; keep them readable. */
export function instanceId(p: RunParams): string {
  const clean = (s: string) => s.replace(/[^A-Za-z0-9_-]/g, "_");
  return `${clean(p.owner)}--${clean(p.repo)}--${p.run}`.slice(0, 100);
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (request.method !== "POST" || url.pathname !== "/runs") {
      return new Response("Not Found", { status: 404 });
    }
    let body: unknown;
    try {
      body = await request.json();
    } catch {
      return new Response("body must be JSON", { status: 400 });
    }
    if (!isRunParams(body)) return new Response("invalid run", { status: 400 });

    const id = instanceId(body);
    try {
      await env.CI_RUN.create({ id, params: body });
    } catch (err) {
      // ripgit may retry a start it thinks failed; the run already exists.
      if (!/already exists/i.test(String(err))) throw err;
    }
    return new Response(id, { status: 202 });
  },
} satisfies ExportedHandler<Env>;
