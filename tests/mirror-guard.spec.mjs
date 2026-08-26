// Divergence guard.
//
// A mirrored repo follows its upstream, so anything pushed to it directly is
// overwritten by the next sweep — silently, with no record the work existed.
// These tests pin the three cases that must stay distinct: a person pushing to
// a mirror is refused, the mirror agent's own push is not, and a repo promoted
// during an outage accepts writes on purpose.

import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  addRemote,
  cleanupTempDirs,
  cloneFixture,
  git,
  gitStdout,
  appendLineAndCommit,
  pushAsOwner,
} from "./helpers/git.mjs";
import { actorHeaders, createTestServer, uniqueId } from "./helpers/mf.mjs";

let server;
const tempDirs = [];

beforeAll(async () => {
  server = await createTestServer();
});

afterAll(async () => {
  await server?.mf.dispose();
  await cleanupTempDirs(tempDirs);
});

/** Push carrying arbitrary trusted actor headers, the way the auth worker does. */
function pushWithHeaders(repoDir, headers, ...args) {
  const config = Object.entries(headers).flatMap(([k, v]) => [
    "-c",
    `http.extraHeader=${k}: ${v}`,
  ]);
  return git(repoDir, [...config, ...args]);
}

async function seedRepo(owner, repo) {
  const source = await cloneFixture();
  tempDirs.push(source.workDir);
  const remoteUrl = new URL(`/${owner}/${repo}`, server.url).toString();
  await addRemote(source.repoDir, "ripgit", remoteUrl);
  await pushAsOwner(source.repoDir, owner, "push", "ripgit", "HEAD:refs/heads/main");
  return { source, remoteUrl };
}

/** A repo mirroring `upstreamUrl`, plus a local clone to push from. */
async function mirroredRepo(owner) {
  const upstream = await seedRepo(owner, "upstream");
  const mirrorUrl = new URL(`/${owner}/mirror`, server.url).toString();

  const resp = await server.dispatch(`/${owner}/mirror/mirror/link`, {
    method: "POST",
    headers: actorHeaders(owner, { "Content-Type": "application/json" }),
    body: JSON.stringify({ remote: upstream.remoteUrl }),
  });
  expect(resp.status).toBe(200);

  await server.dispatch(`/${owner}/mirror/mirror/sync`, {
    method: "POST",
    headers: actorHeaders(owner),
  });

  const work = await cloneFixture();
  tempDirs.push(work.workDir);
  await addRemote(work.repoDir, "mirror", mirrorUrl);

  return { upstream, mirrorUrl, work };
}

const newOwner = () => uniqueId("owner").replace(/-/g, "");

describe("mirror divergence guard", () => {
  test("refuses a direct push to a mirrored repo", async () => {
    const owner = newOwner();
    const { work } = await mirroredRepo(owner);

    await appendLineAndCommit(work.repoDir, "README.md", "local edit", "local: edit");

    await expect(
      pushAsOwner(work.repoDir, owner, "push", "mirror", "HEAD:refs/heads/main"),
    ).rejects.toThrow(/mirrors an upstream|403/);
  });

  test("still accepts the mirror agent's own push", async () => {
    const owner = newOwner();
    const { work } = await mirroredRepo(owner);

    const head = await appendLineAndCommit(
      work.repoDir,
      "README.md",
      "from actions",
      "mirror: delivered by actions",
    );

    // What the OIDC exchange mints: an agent carrying the mirror scope.
    await pushWithHeaders(
      work.repoDir,
      {
        "X-Ripgit-Actor-Name": owner,
        "X-Ripgit-Actor-Scopes": "push,mirror",
      },
      "push",
      "mirror",
      "HEAD:refs/heads/main",
    );

    const refs = await (await server.dispatch(`/${owner}/mirror/refs`)).json();
    expect(refs.heads.main).toBe(head);
  });

  test("accepts writes once promoted, and records the fork point", async () => {
    const owner = newOwner();
    const { work } = await mirroredRepo(owner);

    const before = await (await server.dispatch(`/${owner}/mirror/refs`)).json();

    const promote = await server.dispatch(`/${owner}/mirror/mirror/promote`, {
      method: "POST",
      headers: actorHeaders(owner),
    });
    const promoted = await promote.json();
    expect(promote.status).toBe(200);
    expect(promoted.promoted).toBe(true);
    // The fork point is where reconciliation with upstream has to start.
    expect(promoted.fork_point.refs["refs/heads/main"]).toBe(before.heads.main);

    const head = await appendLineAndCommit(
      work.repoDir,
      "README.md",
      "outage work",
      "local: written during outage",
    );
    await pushAsOwner(work.repoDir, owner, "push", "mirror", "HEAD:refs/heads/main");

    const refs = await (await server.dispatch(`/${owner}/mirror/refs`)).json();
    expect(refs.heads.main).toBe(head);
  });

  test("refuses to sync a promoted repo, so outage work is not fast-forwarded away", async () => {
    const owner = newOwner();
    await mirroredRepo(owner);

    await server.dispatch(`/${owner}/mirror/mirror/promote`, {
      method: "POST",
      headers: actorHeaders(owner),
    });

    const resp = await server.dispatch(`/${owner}/mirror/mirror/sync`, {
      method: "POST",
      headers: actorHeaders(owner),
    });
    expect(resp.status).toBe(409);
  });

  test("demoting restores the guard", async () => {
    const owner = newOwner();
    const { work } = await mirroredRepo(owner);

    await server.dispatch(`/${owner}/mirror/mirror/promote`, {
      method: "POST",
      headers: actorHeaders(owner),
    });
    await server.dispatch(`/${owner}/mirror/mirror/demote`, {
      method: "POST",
      headers: actorHeaders(owner),
    });

    await appendLineAndCommit(work.repoDir, "README.md", "after demote", "local: after demote");
    await expect(
      pushAsOwner(work.repoDir, owner, "push", "mirror", "HEAD:refs/heads/main"),
    ).rejects.toThrow(/mirrors an upstream|403/);
  });

  test("an unmirrored repo takes pushes as before", async () => {
    const owner = newOwner();
    const { source } = await seedRepo(owner, "plain");

    const head = await appendLineAndCommit(
      source.repoDir,
      "README.md",
      "ordinary",
      "local: ordinary push",
    );
    await pushAsOwner(source.repoDir, owner, "push", "ripgit", "HEAD:refs/heads/main");

    const refs = await (await server.dispatch(`/${owner}/plain/refs`)).json();
    expect(refs.heads.main).toBe(head);
  });

  test("promoting a repo that mirrors nothing is a conflict", async () => {
    const owner = newOwner();
    await seedRepo(owner, "plain");

    const resp = await server.dispatch(`/${owner}/plain/mirror/promote`, {
      method: "POST",
      headers: actorHeaders(owner),
    });
    expect(resp.status).toBe(409);
  });
});
