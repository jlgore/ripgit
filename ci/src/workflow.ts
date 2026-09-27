/**
 * One CI run: plan the pipeline in a sandbox, run each job in its own fresh
 * sandbox, and report progress to ripgit as it goes.
 *
 * Durable steps (their names are replay keys; keep them stable):
 *   plan          evaluate the pipeline file for this event
 *   report:plan   tell ripgit the jobs and steps, or that it was skipped
 *   job:<name>    run one job; jobs run in parallel
 *   report:run    the final status
 *
 * A job is one durable step, not one per command: its commands share a
 * sandbox's filesystem, which a replay could not restore. So a job is never
 * retried by the Workflow; a failed job fails the run.
 */

import { getSandbox, type Sandbox } from "@cloudflare/sandbox";
import { WorkflowEntrypoint, type WorkflowEvent, type WorkflowStep } from "cloudflare:workers";
import type { Plan, PlannedJob } from "../sdk/plan-core";
import { RipgitClient, type Report } from "./ripgit";
import type { Env, RunParams } from "./types";

const WORKDIR = "/workspace/repo";
const SDK_DIR = "/opt/ripgit-ci/sdk";
/** Per-step log cap; the rest is cut and noted. */
const MAX_LOG_BYTES = 8 * 1024 * 1024;
const PLAN_TIMEOUT_MS = 60_000;

type JobResult = "success" | "failure";

export class CiRunWorkflow extends WorkflowEntrypoint<Env, RunParams> {
  async run(event: WorkflowEvent<RunParams>, step: WorkflowStep): Promise<JobResult | "skipped" | "error"> {
    const p = event.payload;
    const ripgit = new RipgitClient(this.env.RIPGIT, p.owner, p.repo);
    const key = await runKey(p);

    const plan = await step.do(
      "plan",
      { retries: { limit: 2, delay: "10 seconds", backoff: "exponential" }, timeout: "5 minutes" },
      () => planRun(this.env, ripgit, p, key),
    );

    if (!plan.ok) {
      await step.do("report:run", () =>
        ripgit.report({ type: "run", run: p.run, status: "error", error: plan.error }),
      );
      return "error";
    }

    await step.do("report:plan", () =>
      ripgit.report({
        type: "plan",
        run: p.run,
        name: plan.name,
        matched: plan.matched,
        jobs: plan.jobs.map((j) => ({ name: j.name, steps: j.steps.map((s) => s.name) })),
      }),
    );
    if (!plan.matched) return "skipped";

    const results = await Promise.all(
      plan.jobs.map((job, i) =>
        step.do(
          `job:${job.name}`,
          // Never replayed: see the header. The step outlives the job's own
          // limit so a slow sandbox start cannot cut a job short.
          { retries: { limit: 0, delay: "1 second" }, timeout: job.timeoutMs + 5 * 60_000 },
          () => runJob(this.env, ripgit, p, job, `${key}-${i}`),
        ),
      ),
    );

    const status: JobResult = results.every((r) => r === "success") ? "success" : "failure";
    await step.do("report:run", () => ripgit.report({ type: "run", run: p.run, status }));
    return status;
  }
}

/** A short stable id for this run, used to name its sandboxes. */
async function runKey(p: RunParams): Promise<string> {
  const bytes = new TextEncoder().encode(`${p.owner}/${p.repo}#${p.run}`);
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return "run-" + [...digest.slice(0, 8)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** Run argv in the sandbox to completion; throw unless it exits 0. */
async function mustRun(sandbox: SandboxHandle, argv: [string, ...string[]], cwd = "/"): Promise<string> {
  const proc = await sandbox.exec(argv, { cwd, timeout: 5 * 60_000 });
  const out = await proc.output({ encoding: "utf8", maxBytes: 1024 * 1024 });
  if (out.exitCode !== 0) {
    throw new Error(`${argv.join(" ")} exited ${out.exitCode}: ${(out.stderr || out.stdout).slice(-2000)}`);
  }
  return out.stdout;
}

type SandboxHandle = ReturnType<typeof getSandbox<Sandbox>>;

/** Put the tree at `sha` into WORKDIR. */
async function checkout(sandbox: SandboxHandle, ripgit: RipgitClient, sha: string): Promise<void> {
  await mustRun(sandbox, ["mkdir", "-p", WORKDIR]);
  await sandbox.writeFile("/workspace/source.tar", await ripgit.archive(sha));
  await mustRun(sandbox, ["tar", "-xf", "/workspace/source.tar", "-C", WORKDIR]);
  await mustRun(sandbox, ["rm", "-f", "/workspace/source.tar"]);
}

async function planRun(env: Env, ripgit: RipgitClient, p: RunParams, key: string): Promise<Plan> {
  const sandbox = getSandbox(env.Sandbox, `${key}-plan`);
  try {
    await checkout(sandbox, ripgit, p.sha);
    // Let the pipeline's `import ... from "@ripgit/ci"` resolve to the SDK
    // baked into the image, without the repo having to install anything.
    await mustRun(sandbox, [
      "/bin/bash",
      "-c",
      `mkdir -p ${WORKDIR}/node_modules/@ripgit && [ -e ${WORKDIR}/node_modules/@ripgit/ci ] || ln -s ${SDK_DIR} ${WORKDIR}/node_modules/@ripgit/ci`,
    ]);
    const event = JSON.stringify({ event: p.event, ref: p.ref, sha: p.sha });
    const proc = await sandbox.exec(["bun", `${SDK_DIR}/plan.ts`, p.pipeline, event], {
      cwd: WORKDIR,
      timeout: PLAN_TIMEOUT_MS,
    });
    const out = await proc.output({ encoding: "utf8", maxBytes: 1024 * 1024 });
    if (out.timedOut) return { ok: false, error: `evaluating ${p.pipeline} took over ${PLAN_TIMEOUT_MS / 1000}s` };
    const last = out.stdout.trim().split("\n").pop() ?? "";
    try {
      return JSON.parse(last) as Plan;
    } catch {
      return {
        ok: false,
        error: `evaluating ${p.pipeline} failed (exit ${out.exitCode}): ${(out.stderr || out.stdout).slice(-2000)}`,
      };
    }
  } finally {
    await sandbox.destroy();
  }
}

async function runJob(
  env: Env,
  ripgit: RipgitClient,
  p: RunParams,
  job: PlannedJob,
  sandboxId: string,
): Promise<JobResult> {
  // Progress reports are best-effort inside a job: a lost one leaves a stale
  // status on the page, but must not fail the build. The final run report is
  // its own durable step.
  const report = (r: Report) =>
    ripgit.report(r).catch((err) => console.error(`ci: ${String(err)}`));
  const sandbox = getSandbox(env.Sandbox, sandboxId);
  const deadline = Date.now() + job.timeoutMs;

  await report({ type: "job", run: p.run, job: job.name, status: "running" });
  let result: JobResult = "success";
  try {
    await checkout(sandbox, ripgit, p.sha);
    for (const [index, s] of job.steps.entries()) {
      if (result === "failure") {
        await report({ type: "step", run: p.run, job: job.name, index, status: "skipped" });
        continue;
      }
      const remaining = deadline - Date.now();
      if (remaining <= 0) {
        await report({ type: "step", run: p.run, job: job.name, index, status: "cancelled" });
        result = "failure";
        continue;
      }
      await report({ type: "step", run: p.run, job: job.name, index, status: "running" });

      // stderr folds into stdout so the log keeps its order.
      const script = `exec 2>&1\nset -eo pipefail\n${s.run}`;
      const proc = await sandbox.exec(["/bin/bash", "-c", script], {
        cwd: WORKDIR,
        env: { CI: "true", RIPGIT_CI: "true", RIPGIT_SHA: p.sha, RIPGIT_REF: p.ref, ...job.env },
        timeout: remaining,
      });
      const out = await proc.output({ encoding: "utf8", maxBytes: MAX_LOG_BYTES });
      let log = out.stdout;
      if (out.truncated) log += `\n[ripgit-ci] log cut at ${MAX_LOG_BYTES} bytes\n`;
      if (out.timedOut) log += `\n[ripgit-ci] job timed out after ${job.timeoutMs / 1000}s\n`;

      const logKey = `${p.owner}/${p.repo}/${p.run}/${job.name}/${index}.log`;
      await env.CI_LOGS.put(logKey, log, { httpMetadata: { contentType: "text/plain; charset=utf-8" } });

      const ok = out.exitCode === 0 && !out.timedOut;
      if (!ok) result = "failure";
      await report({
        type: "step",
        run: p.run,
        job: job.name,
        index,
        status: ok ? "success" : "failure",
        exitCode: out.exitCode,
        logKey,
      });
    }
  } catch (err) {
    // Infrastructure, not the build: say so in the first unfinished step's log.
    result = "failure";
    console.error(`ci: job ${job.name} of run ${p.run}: ${String(err)}`);
    const logKey = `${p.owner}/${p.repo}/${p.run}/${job.name}/runner.log`;
    await env.CI_LOGS.put(logKey, `[ripgit-ci] ${String(err)}\n`);
    await report({ type: "run", run: p.run, status: "error", error: `job ${job.name}: ${String(err)}` });
  } finally {
    await sandbox.destroy().catch(() => undefined);
  }
  await report({ type: "job", run: p.run, job: job.name, status: result });
  return result;
}
