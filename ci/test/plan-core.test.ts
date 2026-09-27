import { describe, expect, test } from "vitest";
import { pipeline } from "../sdk/index.ts";
import {
  branchMatches,
  DEFAULT_JOB_TIMEOUT_MS,
  MAX_JOB_TIMEOUT_MS,
  parseDuration,
  planPipeline,
  type CiEvent,
} from "../sdk/plan-core.ts";

const push = (branch: string): CiEvent => ({ event: "push", ref: `refs/heads/${branch}`, sha: "a".repeat(40) });

const basic = pipeline({
  name: "test",
  on: { push: { branches: ["main", "release/*"] } },
  jobs: {
    test: { steps: [{ name: "build", run: "cargo build" }, { name: "test", run: "cargo test" }] },
    lint: { timeout: "5 minutes", env: { RUSTFLAGS: "-D warnings" }, steps: [{ name: "clippy", run: "cargo clippy" }] },
  },
});

describe("planPipeline", () => {
  test("plans every job, in order, with defaults filled in", () => {
    const plan = planPipeline(basic, push("main"));
    expect(plan).toEqual({
      ok: true,
      name: "test",
      matched: true,
      jobs: [
        {
          name: "test",
          timeoutMs: DEFAULT_JOB_TIMEOUT_MS,
          env: {},
          steps: [
            { name: "build", run: "cargo build" },
            { name: "test", run: "cargo test" },
          ],
        },
        { name: "lint", timeoutMs: 5 * 60_000, env: { RUSTFLAGS: "-D warnings" }, steps: [{ name: "clippy", run: "cargo clippy" }] },
      ],
    });
  });

  test("an unmatched branch still plans, but says it did not match", () => {
    const plan = planPipeline(basic, push("feature/x"));
    expect(plan.ok && plan.matched).toBe(false);
    expect(plan.ok && planPipeline(basic, push("release/1.2")).ok).toBe(true);
    const release = planPipeline(basic, push("release/1.2"));
    expect(release.ok && release.matched).toBe(true);
  });

  test("push without branches matches every branch; tags never match", () => {
    const any = { ...basic, on: { push: {} } };
    const plan = planPipeline(any, push("anything/at-all"));
    expect(plan.ok && plan.matched).toBe(true);
    const tag = planPipeline(any, { event: "push", ref: "refs/tags/v1", sha: "a".repeat(40) });
    expect(tag.ok && tag.matched).toBe(false);
  });

  test("a pipeline with no push trigger never runs on push", () => {
    const plan = planPipeline({ ...basic, on: {} }, push("main"));
    expect(plan.ok && plan.matched).toBe(false);
  });

  test("explains what is wrong with a bad definition", () => {
    const error = (def: unknown) => {
      const plan = planPipeline(def, push("main"));
      return plan.ok ? null : plan.error;
    };
    expect(error(undefined)).toContain("export default pipeline");
    expect(error({ ...basic, name: "" })).toContain("`name`");
    expect(error({ ...basic, jobs: {} })).toContain("at least one job");
    expect(error({ ...basic, jobs: { "bad name": { steps: [{ name: "x", run: "true" }] } } })).toContain("job name");
    expect(error({ ...basic, jobs: { a: { steps: [] } } })).toContain("at least one step");
    expect(error({ ...basic, jobs: { a: { steps: [{ name: "x", run: " " }] } } })).toContain("`run`");
    expect(error({ ...basic, jobs: { a: { timeout: "soon", steps: [{ name: "x", run: "true" }] } } })).toContain("timeout");
    expect(error({ ...basic, jobs: { a: { env: { N: 1 }, steps: [{ name: "x", run: "true" }] } } })).toContain("env");
  });

  test("steps without names are numbered; timeouts are capped", () => {
    const plan = planPipeline(
      { name: "p", on: { push: {} }, jobs: { a: { timeout: "48h", steps: [{ run: "true" }] } } },
      push("main"),
    );
    expect(plan.ok && plan.jobs[0].steps[0].name).toBe("step 1");
    expect(plan.ok && plan.jobs[0].timeoutMs).toBe(MAX_JOB_TIMEOUT_MS);
  });
});

describe("helpers", () => {
  test("parseDuration reads the common spellings", () => {
    expect(parseDuration("20 minutes")).toBe(20 * 60_000);
    expect(parseDuration("90s")).toBe(90_000);
    expect(parseDuration("1h")).toBe(3_600_000);
    expect(parseDuration("1500ms")).toBe(1500);
    expect(parseDuration("1.5 hours")).toBe(5_400_000);
    expect(parseDuration("soon")).toBeNull();
  });

  test("branchMatches treats * as a wildcard and everything else literally", () => {
    expect(branchMatches("main", "main")).toBe(true);
    expect(branchMatches("main", "mainline")).toBe(false);
    expect(branchMatches("release/*", "release/1.0")).toBe(true);
    expect(branchMatches("v1.0", "v1x0")).toBe(false);
    expect(branchMatches("*", "anything/nested")).toBe(true);
  });
});
