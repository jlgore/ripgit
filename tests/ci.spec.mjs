// CI, ripgit's side: starting runs on push, the source archive runners fetch,
// status reports, and the Actions pages. ripgit-ci itself is stubbed: the
// stub records the runs ripgit asks it to start.

import { execFileSync } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  addRemote,
  cleanupTempDirs,
  cloneFixture,
  git,
  gitStdout,
  makeTempDir,
  pushAsOwner,
} from "./helpers/git.mjs";
import {
  actorHeaders,
  ciHeaders,
  createTestServer,
  mirrorAgentHeaders,
  uniqueId,
} from "./helpers/mf.mjs";

let server;
const tempDirs = [];

beforeAll(async () => {
  server = await createTestServer();
});

afterAll(async () => {
  await server?.mf.dispose();
  await cleanupTempDirs(tempDirs);
});

const newName = (prefix) => uniqueId(prefix).replace(/-/g, "");

const PIPELINE = `import { pipeline } from "@ripgit/ci";
export default pipeline({
  name: "test",
  on: { push: { branches: ["main"] } },
  jobs: { test: { steps: [{ name: "test", run: "cargo test" }] } },
});
`;

/** The fixture plus pipeline files, committed and pushed to owner/repo. */
async function pushWithPipelines(owner, repo, pipelines, headers) {
  const source = await cloneFixture();
  tempDirs.push(source.workDir);
  if (pipelines.length) {
    const dir = join(source.repoDir, ".ripgit", "pipelines");
    await mkdir(dir, { recursive: true });
    for (const name of pipelines) await writeFile(join(dir, name), PIPELINE);
    await git(source.repoDir, ["add", "."]);
    await git(source.repoDir, ["commit", "-q", "-m", "add pipelines"]);
  }
  await addRemote(source.repoDir, "ripgit", new URL(`/${owner}/${repo}`, server.url).toString());
  if (headers) {
    const config = Object.entries(headers).flatMap(([k, v]) => ["-c", `http.extraHeader=${k}: ${v}`]);
    await git(source.repoDir, [...config, "push", "ripgit", "HEAD:refs/heads/main"]);
  } else {
    await pushAsOwner(source.repoDir, owner, "push", "ripgit", "HEAD:refs/heads/main");
  }
  const sha = await gitStdout(source.repoDir, ["rev-parse", "HEAD"]);
  return { source, sha };
}

async function runsFor(owner) {
  return (await server.startedRuns()).filter((r) => r.owner === owner);
}

async function report(owner, repo, body, headers = ciHeaders(`${owner}/${repo}`)) {
  const resp = await server.dispatch(`/${owner}/${repo}/ci/report`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return { status: resp.status, text: await resp.text() };
}

async function page(path, headers = {}) {
  const resp = await server.dispatch(path, { headers: { Accept: "text/markdown", ...headers } });
  return { status: resp.status, text: await resp.text() };
}

describe("starting runs", () => {
  test("a push starts one run per pipeline file at the new commit", async () => {
    const owner = newName("owner");
    const { sha } = await pushWithPipelines(owner, "repo", ["lint.ts", "test.ts"]);

    const runs = await runsFor(owner);
    expect(runs).toEqual([
      { owner, repo: "repo", run: 1, sha, ref: "refs/heads/main", event: "push", pipeline: ".ripgit/pipelines/lint.ts" },
      { owner, repo: "repo", run: 2, sha, ref: "refs/heads/main", event: "push", pipeline: ".ripgit/pipelines/test.ts" },
    ]);

    const list = await page(`/${owner}/repo/actions`);
    expect(list.status).toBe(200);
    expect(list.text).toContain("#1 `.ripgit/pipelines/lint.ts` - **planning**");
    expect(list.text).toContain("#2 `.ripgit/pipelines/test.ts` - **planning**");
  });

  test("a push without pipelines starts nothing", async () => {
    const owner = newName("owner");
    await pushWithPipelines(owner, "repo", []);
    expect(await runsFor(owner)).toEqual([]);
  });

  test("a mirror's push starts nothing: upstream already ran CI", async () => {
    const owner = newName("owner");
    await pushWithPipelines(owner, "repo", ["test.ts"], mirrorAgentHeaders(`${owner}/repo`));
    expect(await runsFor(owner)).toEqual([]);
  });
});

describe("reports", () => {
  test("move a run through plan, jobs, and steps, and keep failure output", async () => {
    const owner = newName("owner");
    await pushWithPipelines(owner, "repo", ["test.ts"]);

    const ok = async (body) => expect((await report(owner, "repo", body)).status).toBe(200);
    await ok({ type: "plan", run: 1, name: "test", matched: true, jobs: [{ name: "test", steps: ["build", "test"] }] });
    await ok({ type: "job", run: 1, job: "test", status: "running" });
    expect((await page(`/${owner}/repo/actions`)).text).toContain("**running**");

    const buildLog = `${owner}/repo/1/test/0.log`;
    const testLog = `${owner}/repo/1/test/1.log`;
    await server.logs.put(buildLog, "Compiling ripgit\nFinished\n");
    await server.logs.put(testLog, `${"noise\n".repeat(300)}thread 'main' panicked: assertion failed\n`);
    await ok({ type: "step", run: 1, job: "test", index: 0, status: "success", exitCode: 0, logKey: buildLog });
    await ok({ type: "step", run: 1, job: "test", index: 1, status: "failure", exitCode: 101, logKey: testLog });
    await ok({ type: "job", run: 1, job: "test", status: "failure" });
    await ok({ type: "run", run: 1, status: "failure" });

    const detail = await page(`/${owner}/repo/actions/1`);
    expect(detail.text).toContain("- Status: **failure**");
    expect(detail.text).toContain("- 2. `test`: failure (exit 101)");
    // The failing step's tail is inline, without the whole log.
    expect(detail.text).toContain("assertion failed");
    expect(detail.text.split("noise").length - 1).toBeLessThan(300);

    const raw = await server.dispatch(`/${owner}/repo/actions/1/logs/test/0`);
    expect(raw.headers.get("Content-Type")).toContain("text/plain");
    expect(await raw.text()).toBe("Compiling ripgit\nFinished\n");

    const html = await server.dispatch(`/${owner}/repo/actions/1`, { headers: { Accept: "text/html" } });
    expect(html.status).toBe(200);
    expect(await html.text()).toContain("assertion failed");

    // A finished run is final: a late report cannot reopen it.
    await ok({ type: "run", run: 1, status: "success" });
    expect((await page(`/${owner}/repo/actions/1`)).text).toContain("- Status: **failure**");
  });

  test("a plan whose triggers do not match skips the run", async () => {
    const owner = newName("owner");
    await pushWithPipelines(owner, "repo", ["test.ts"]);
    expect((await report(owner, "repo", { type: "plan", run: 1, name: "test", matched: false })).status).toBe(200);
    expect((await page(`/${owner}/repo/actions`)).text).toContain("**skipped**");
  });

  test("only ripgit-ci, scoped to this repo, may report", async () => {
    const owner = newName("owner");
    await pushWithPipelines(owner, "repo", ["test.ts"]);
    const body = { type: "run", run: 1, status: "success" };

    expect((await report(owner, "repo", body, actorHeaders(owner))).status).toBe(403);
    expect((await report(owner, "repo", body, ciHeaders(`${owner}/other`))).status).toBe(403);
    expect((await report(owner, "repo", body, {})).status).toBe(403);
    expect((await page(`/${owner}/repo/actions/1`)).text).toContain("**planning**");
  });

  test("nonsense reports are refused", async () => {
    const owner = newName("owner");
    await pushWithPipelines(owner, "repo", ["test.ts"]);
    expect((await report(owner, "repo", { type: "run", run: 99, status: "success" })).status).toBe(400);
    expect((await report(owner, "repo", { type: "run", run: 1, status: "great" })).status).toBe(400);
    expect((await report(owner, "repo", { type: "bogus", run: 1 })).status).toBe(400);
  });
});

describe("archive", () => {
  test("matches git archive for the same commit, and follows repo visibility", async () => {
    const owner = newName("owner");
    const { source, sha } = await pushWithPipelines(owner, "repo", ["test.ts"]);

    // Private: only callers with a role (here ripgit-ci for this repo) get it.
    const vis = await server.dispatch(`/${owner}/repo/settings/visibility`, {
      method: "POST",
      headers: actorHeaders(owner, { "Content-Type": "application/x-www-form-urlencoded" }),
      body: "visibility=private",
      redirect: "manual",
    });
    expect(vis.status).toBe(302);
    const anonymous = await server.dispatch(`/${owner}/repo/archive/${sha}`);
    expect(anonymous.status).toBe(404);
    await anonymous.arrayBuffer();

    const resp = await server.dispatch(`/${owner}/repo/archive/${sha}`, {
      headers: ciHeaders(`${owner}/repo`),
    });
    expect(resp.status).toBe(200);
    expect(resp.headers.get("Content-Type")).toBe("application/x-tar");
    const tar = Buffer.from(await resp.arrayBuffer());

    const ours = await makeTempDir("ci-archive-ours-");
    const theirs = await makeTempDir("ci-archive-git-");
    tempDirs.push(ours, theirs);
    execFileSync("tar", ["-x", "-C", ours], { input: tar });
    const gitTar = execFileSync("git", ["-C", source.repoDir, "archive", "--format=tar", sha], {
      maxBuffer: 256 * 1024 * 1024,
    });
    execFileSync("tar", ["-x", "-C", theirs], { input: gitTar });
    // Same files, same bytes, same executable bits and symlinks.
    try {
      execFileSync("diff", ["-r", "--no-dereference", ours, theirs]);
    } catch (err) {
      throw new Error(`archive differs from git archive:\n${err.stdout}`);
    }
    const modes = (dir) =>
      execFileSync("find", [dir, "-type", "f", "-perm", "-u+x", "-printf", "%P\\n"]).toString().split("\n").sort();
    expect(modes(ours)).toEqual(modes(theirs));

    const missing = await server.dispatch(`/${owner}/repo/archive/${"0".repeat(40)}`, {
      headers: ciHeaders(`${owner}/repo`),
    });
    expect(missing.status).toBe(404);
    await missing.arrayBuffer();
  });
});
