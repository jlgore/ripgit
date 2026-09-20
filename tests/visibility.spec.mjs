// Repo visibility.
//
// A mirror inherits its upstream's visibility from the signed OIDC claim the
// mirror agent forwards. A private mirror must not be readable by anyone but
// its owner -- not through the pages, not through the API, and not by cloning.

import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  addRemote,
  cleanupTempDirs,
  cloneFixture,
  git,
  makeTempDir,
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

function pushWithHeaders(repoDir, headers, ...args) {
  const config = Object.entries(headers).flatMap(([k, v]) => [
    "-c",
    `http.extraHeader=${k}: ${v}`,
  ]);
  return git(repoDir, [...config, ...args]);
}

/** Push the fixture to `owner/repo`, declaring the upstream's visibility. */
async function seed(owner, repo, visibility) {
  const source = await cloneFixture();
  tempDirs.push(source.workDir);
  const url = new URL(`/${owner}/${repo}`, server.url).toString();
  await addRemote(source.repoDir, "ripgit", url);

  const headers = { "X-Ripgit-Actor-Name": owner };
  if (visibility) headers["X-Ripgit-Repo-Visibility"] = visibility;
  await pushWithHeaders(source.repoDir, headers, "push", "ripgit", "HEAD:refs/heads/main");

  return { source, url };
}

const newOwner = () => uniqueId("owner").replace(/-/g, "");

describe("repo visibility", () => {
  test("a private mirror is invisible to anonymous readers", async () => {
    const owner = newOwner();
    await seed(owner, "secret", "private");

    for (const path of ["/", "/refs", "/commits", "/stats"]) {
      const resp = await server.dispatch(`/${owner}/secret${path}`);
      // 404 rather than 403: the name itself is something the upstream withheld.
      expect(resp.status, `path ${path}`).toBe(404);
    }
  });

  test("its owner can still read it", async () => {
    const owner = newOwner();
    await seed(owner, "secret", "private");

    const resp = await server.dispatch(`/${owner}/secret/refs`, {
      headers: actorHeaders(owner),
    });
    expect(resp.status).toBe(200);
    expect((await resp.json()).heads.main).toBeTruthy();
  });

  test("another signed-in user is refused", async () => {
    const owner = newOwner();
    await seed(owner, "secret", "private");

    const resp = await server.dispatch(`/${owner}/secret/refs`, {
      headers: actorHeaders("someone-else"),
    });
    expect(resp.status).toBe(404);
  });

  test("a private mirror cannot be cloned anonymously", async () => {
    const owner = newOwner();
    const { url } = await seed(owner, "secret", "private");
    const dir = await makeTempDir("ripgit-clone");
    tempDirs.push(dir);

    await expect(git(dir, ["clone", url, "cloned"])).rejects.toThrow();
  });

  test("a public mirror stays readable by anyone", async () => {
    const owner = newOwner();
    await seed(owner, "open", "public");

    const resp = await server.dispatch(`/${owner}/open/refs`);
    expect(resp.status).toBe(200);
  });

  test("a push with no visibility header stays readable", async () => {
    // Ordinary pushes carry no claim, and repos that predate visibility
    // tracking must not become unreachable.
    const owner = newOwner();
    await seed(owner, "legacy", null);

    const resp = await server.dispatch(`/${owner}/legacy/refs`);
    expect(resp.status).toBe(200);
  });

  test("the owner profile hides private repos from strangers", async () => {
    const owner = newOwner();
    await seed(owner, "open", "public");
    await seed(owner, "secret", "private");

    const anon = await (
      await server.dispatch(`/${owner}/`, { headers: { Accept: "text/markdown" } })
    ).text();
    expect(anon).toContain("open");
    expect(anon).not.toContain("secret");

    const mine = await (
      await server.dispatch(`/${owner}/`, {
        headers: actorHeaders(owner, { Accept: "text/markdown" }),
      })
    ).text();
    expect(mine).toContain("open");
    expect(mine).toContain("secret");
  });
});
