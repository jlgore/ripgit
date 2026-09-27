/**
 * Turning a pipeline definition into a plan: validation, trigger matching,
 * and timeouts. Pure, so the worker's tests can run it without a sandbox.
 */

import type { Pipeline } from "./index.ts";

export interface CiEvent {
  event: "push";
  /** e.g. "refs/heads/main" */
  ref: string;
  sha: string;
}

export interface PlannedStep {
  name: string;
  run: string;
}

export interface PlannedJob {
  name: string;
  timeoutMs: number;
  env: Record<string, string>;
  steps: PlannedStep[];
}

export type Plan =
  | { ok: true; name: string; matched: boolean; jobs: PlannedJob[] }
  | { ok: false; error: string };

export const DEFAULT_JOB_TIMEOUT_MS = 30 * 60_000;
export const MAX_JOB_TIMEOUT_MS = 6 * 60 * 60_000;
const JOB_NAME = /^[A-Za-z0-9._-]{1,64}$/;

/** "20 minutes", "90s", "1h", "1500ms" → milliseconds; null if unreadable. */
export function parseDuration(text: string): number | null {
  const m = /^\s*(\d+(?:\.\d+)?)\s*(ms|milliseconds?|s|secs?|seconds?|m|mins?|minutes?|h|hrs?|hours?)\s*$/i.exec(text);
  if (!m) return null;
  const n = Number(m[1]);
  const unit = m[2].toLowerCase();
  if (unit.startsWith("ms") || unit.startsWith("milli")) return n;
  if (unit.startsWith("s")) return n * 1000;
  if (unit.startsWith("m")) return n * 60_000;
  return n * 3_600_000;
}

/** Whether `branch` matches `pattern`; `*` matches any run of characters. */
export function branchMatches(pattern: string, branch: string): boolean {
  const re = new RegExp(
    "^" + pattern.split("*").map((p) => p.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join(".*") + "$",
  );
  return re.test(branch);
}

export function triggerMatches(definition: Pipeline, event: CiEvent): boolean {
  if (event.event !== "push") return false;
  const push = definition.on?.push;
  if (!push) return false;
  const branch = event.ref.startsWith("refs/heads/") ? event.ref.slice("refs/heads/".length) : null;
  if (branch === null) return false;
  if (!push.branches) return true;
  return push.branches.some((pattern) => branchMatches(pattern, branch));
}

function isRecordOfStrings(value: unknown): value is Record<string, string> {
  return (
    typeof value === "object" &&
    value !== null &&
    Object.values(value).every((v) => typeof v === "string")
  );
}

/** Validate a pipeline's default export and plan it for `event`. */
export function planPipeline(definition: unknown, event: CiEvent): Plan {
  const fail = (error: string): Plan => ({ ok: false, error });
  if (typeof definition !== "object" || definition === null) {
    return fail("the pipeline file must `export default pipeline({...})`");
  }
  const p = definition as Pipeline;
  if (typeof p.name !== "string" || !p.name.trim()) return fail("`name` must be a non-empty string");
  if (typeof p.on !== "object" || p.on === null) return fail("`on` must say when the pipeline runs");
  if (typeof p.jobs !== "object" || p.jobs === null || Object.keys(p.jobs).length === 0) {
    return fail("`jobs` must define at least one job");
  }

  const jobs: PlannedJob[] = [];
  for (const [name, job] of Object.entries(p.jobs)) {
    if (!JOB_NAME.test(name)) {
      return fail(`job name "${name}" must be 1-64 letters, digits, ".", "_", or "-"`);
    }
    if (!Array.isArray(job?.steps) || job.steps.length === 0) {
      return fail(`job "${name}" needs at least one step`);
    }
    const steps: PlannedStep[] = [];
    for (const [i, step] of job.steps.entries()) {
      if (typeof step?.run !== "string" || !step.run.trim()) {
        return fail(`job "${name}" step ${i + 1} needs a \`run\` script`);
      }
      steps.push({ name: typeof step.name === "string" && step.name ? step.name : `step ${i + 1}`, run: step.run });
    }
    let timeoutMs = DEFAULT_JOB_TIMEOUT_MS;
    if (job.timeout !== undefined) {
      const parsed = typeof job.timeout === "string" ? parseDuration(job.timeout) : null;
      if (parsed === null || parsed <= 0) return fail(`job "${name}" has an unreadable timeout "${job.timeout}"`);
      timeoutMs = Math.min(parsed, MAX_JOB_TIMEOUT_MS);
    }
    if (job.env !== undefined && !isRecordOfStrings(job.env)) {
      return fail(`job "${name}" env values must be strings`);
    }
    jobs.push({ name, timeoutMs, env: job.env ?? {}, steps });
  }

  return { ok: true, name: p.name, matched: triggerMatches(p, event), jobs };
}
