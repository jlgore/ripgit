// Mirror sync, end to end.
//
// ripgit is itself a git server, so a second repo in the same instance stands
// in for the upstream. That exercises the whole client path — ref discovery,
// upload-pack negotiation, side-band demux, pack ingest, ref update, FTS
// rebuild — against real git objects, with no network and no credentials.

import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  addRemote,
  cleanupTempDirs,
  cloneFixture,
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

/** Push the fixture repo to `owner/repo` and return its HEAD sha. */
async function seedUpstream(owner, repo) {
  const source = await cloneFixture();
  tempDirs.push(source.workDir);

  const remoteUrl = new URL(`/${owner}/${repo}`, server.url).toString();
  await addRemote(source.repoDir, "ripgit", remoteUrl);
  await pushAsOwner(source.repoDir, owner, "push", "ripgit", "HEAD:refs/heads/main");

  return {
    source,
    remoteUrl,
    head: await gitStdout(source.repoDir, ["rev-parse", "HEAD"]),
  };
}

async function link(owner, repo, remote) {
  const resp = await server.dispatch(`/${owner}/${repo}/mirror/link`, {
    method: "POST",
    headers: actorHeaders(owner, { "Content-Type": "application/json" }),
    body: JSON.stringify({ remote }),
  });
  expect(resp.status).toBe(200);
  return resp.json();
}

async function sync(owner, repo) {
  const resp = await server.dispatch(`/${owner}/${repo}/mirror/sync`, {
    method: "POST",
    headers: actorHeaders(owner),
  });
  const body = await resp.json();
  expect(resp.status, JSON.stringify(body)).toBe(200);
  return body;
}

describe("github mirror sync", () => {
  test("clones an upstream repo into an empty mirror", async () => {
    const owner = uniqueId("owner").replace(/-/g, "");
    const upstream = await seedUpstream(owner, "upstream");
    const mirror = "mirror";

    await link(owner, mirror, upstream.remoteUrl);
    const report = await sync(owner, mirror);

    expect(report.updated).toContain("refs/heads/main");
    expect(report.pack_bytes).toBeGreaterThan(0);
    // First sync adopts a default branch, so search is indexed too.
    expect(report.reindexed).toBe(true);

    // The mirror's ref must point at exactly the upstream commit.
    const refs = await (await server.dispatch(`/${owner}/${mirror}/refs`)).json();
    expect(refs.heads.main).toBe(upstream.head);

    // HEAD is advertised by every remote but is a pointer, not a ref to store.
    // Storing it would make this mirror advertise HEAD twice once it is served.
    expect(report.updated).not.toContain("HEAD");
    expect(Object.keys(refs.heads)).not.toContain("HEAD");
  });

  test("re-syncing an unchanged upstream transfers nothing", async () => {
    const owner = uniqueId("owner").replace(/-/g, "");
    const upstream = await seedUpstream(owner, "upstream");

    await link(owner, "mirror", upstream.remoteUrl);
    await sync(owner, "mirror");
    const second = await sync(owner, "mirror");

    // Everything already matches, so negotiation should not ask for a pack.
    expect(second.updated).toEqual([]);
    expect(second.unchanged).toContain("refs/heads/main");
    expect(second.pack_bytes).toBe(0);
  });

  test("picks up new upstream commits incrementally", async () => {
    const owner = uniqueId("owner").replace(/-/g, "");
    const upstream = await seedUpstream(owner, "upstream");

    await link(owner, "mirror", upstream.remoteUrl);
    const first = await sync(owner, "mirror");

    const newHead = await appendLineAndCommit(
      upstream.source.repoDir,
      "README.md",
      "mirrored change",
      "mirror: incremental commit",
    );
    await pushAsOwner(
      upstream.source.repoDir,
      owner,
      "push",
      "ripgit",
      "HEAD:refs/heads/main",
    );

    const second = await sync(owner, "mirror");

    expect(second.updated).toContain("refs/heads/main");
    const refs = await (await server.dispatch(`/${owner}/mirror/refs`)).json();
    expect(refs.heads.main).toBe(newHead);

    // `have` negotiation means the second pack carries one commit, not the
    // whole history — the point of incremental sync.
    expect(second.pack_bytes).toBeLessThan(first.pack_bytes);
  });

  test("syncing without a linked upstream is a conflict, not a crash", async () => {
    const owner = uniqueId("owner").replace(/-/g, "");
    const resp = await server.dispatch(`/${owner}/unlinked/mirror/sync`, {
      method: "POST",
      headers: actorHeaders(owner),
    });
    expect(resp.status).toBe(409);
  });

  test("linking and syncing require repo ownership", async () => {
    const owner = uniqueId("owner").replace(/-/g, "");
    const upstream = await seedUpstream(owner, "upstream");
    await link(owner, "mirror", upstream.remoteUrl);

    const stranger = await server.dispatch(`/${owner}/mirror/mirror/sync`, {
      method: "POST",
      headers: actorHeaders("someone-else"),
    });
    expect(stranger.status).toBe(403);
  });
});
