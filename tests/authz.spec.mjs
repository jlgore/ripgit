// Role resolution.
//
// Who may do what is decided in ripgit's Worker from the DIRECTORY database
// (better-auth's orgs and teams, ripgit's namespaces and grants) and handed to
// the repo's Durable Object as X-Ripgit-Role. These tests seed that database
// directly, the way the auth worker would populate it.

import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  addRemote,
  appendLineAndCommit,
  cleanupTempDirs,
  cloneFixture,
  git,
} from "./helpers/git.mjs";
import {
  actorHeaders,
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
const userId = (name) => `test-user:${name}`;

/** Push the fixture to `owner/repo` as `pusher`, with any extra headers. */
async function push(owner, repo, headers) {
  const source = await cloneFixture();
  tempDirs.push(source.workDir);
  await addRemote(
    source.repoDir,
    "ripgit",
    new URL(`/${owner}/${repo}`, server.url).toString(),
  );
  const config = Object.entries(headers).flatMap(([k, v]) => [
    "-c",
    `http.extraHeader=${k}: ${v}`,
  ]);
  await git(source.repoDir, [...config, "push", "ripgit", "HEAD:refs/heads/main"]);
  return source;
}

/** Try a follow-up push as someone else; resolves to git's success. */
async function pushesAs(source, headers) {
  await appendLineAndCommit(source.repoDir, "README.md", uniqueId("line"), "change");
  const config = Object.entries(headers).flatMap(([k, v]) => [
    "-c",
    `http.extraHeader=${k}: ${v}`,
  ]);
  try {
    await git(source.repoDir, [...config, "push", "ripgit", "HEAD:refs/heads/main"]);
    return true;
  } catch {
    return false;
  }
}

async function settingsStatus(owner, repo, headers) {
  const resp = await server.dispatch(`/${owner}/${repo}/settings`, { headers });
  await resp.arrayBuffer();
  return resp.status;
}

async function seedUser(name) {
  await server.db
    .prepare(
      `INSERT OR IGNORE INTO "user" (id, name, email, "emailVerified", "createdAt", "updatedAt", login)
       VALUES (?, ?, ?, 1, ?, ?, ?)`,
    )
    .bind(userId(name), name, `${name}@example.test`, Date.now(), Date.now(), name)
    .run();
}

/** An org namespace with the given members ({ name: "owner" | "admin" | "member" }). */
async function seedOrg(slug, members, defaultRepoRole) {
  const orgId = `org:${slug}`;
  await server.db
    .prepare(`INSERT INTO organization (id, name, slug, "createdAt") VALUES (?, ?, ?, ?)`)
    .bind(orgId, slug, slug, Date.now())
    .run();
  await server.db
    .prepare(
      "INSERT INTO ripgit_namespaces (name, kind, org_id, created_at) VALUES (?, 'org', ?, ?)",
    )
    .bind(slug, orgId, Date.now())
    .run();
  if (defaultRepoRole) {
    await server.db
      .prepare("INSERT INTO ripgit_org_settings (org_id, default_repo_role) VALUES (?, ?)")
      .bind(orgId, defaultRepoRole)
      .run();
  }
  for (const [name, role] of Object.entries(members)) {
    await seedUser(name);
    await server.db
      .prepare(
        `INSERT INTO member (id, "organizationId", "userId", role, "createdAt") VALUES (?, ?, ?, ?, ?)`,
      )
      .bind(`member:${slug}:${name}`, orgId, userId(name), role, Date.now())
      .run();
  }
  return orgId;
}

async function grant(repo, granteeKind, granteeId, role) {
  await server.db
    .prepare(
      "INSERT INTO ripgit_repo_grants (repo, grantee_kind, grantee_id, role) VALUES (?, ?, ?, ?)",
    )
    .bind(repo, granteeKind, granteeId, role)
    .run();
}

describe("user namespaces", () => {
  test("only the namespace owner may push or open settings", async () => {
    const owner = newName("owner");
    const source = await push(owner, "repo", actorHeaders(owner));

    expect(await pushesAs(source, actorHeaders(newName("stranger")))).toBe(false);
    expect(await settingsStatus(owner, "repo", actorHeaders(newName("stranger")))).toBe(403);
    expect(await settingsStatus(owner, "repo", actorHeaders(owner))).toBe(200);
  });

  test("a role header from outside is overwritten, not trusted", async () => {
    const owner = newName("owner");
    await push(owner, "repo", actorHeaders(owner));

    const forged = actorHeaders(newName("stranger"), { "X-Ripgit-Role": "admin" });
    expect(await settingsStatus(owner, "repo", forged)).toBe(403);
    // Anonymous, too: a forged header must not turn a 401 into access.
    expect(await settingsStatus(owner, "repo", { "X-Ripgit-Role": "admin" })).toBe(401);
  });

  test("a repo grant gives exactly the granted role", async () => {
    const owner = newName("owner");
    const writer = newName("writer");
    const source = await push(owner, "repo", actorHeaders(owner));
    await grant(`${owner}/repo`, "user", userId(writer), "write");

    expect(await pushesAs(source, actorHeaders(writer))).toBe(true);
    // write is not admin
    expect(await settingsStatus(owner, "repo", actorHeaders(writer))).toBe(403);
  });

  test("a namespace nobody has claimed gives nobody write access", async () => {
    const owner = newName("owner");
    const intruder = newName("intruder");
    const source = await cloneFixture();
    tempDirs.push(source.workDir);
    await addRemote(
      source.repoDir,
      "ripgit",
      new URL(`/${owner}/repo`, server.url).toString(),
    );
    // The gateway claims only the pusher's own name, so `owner` stays unclaimed.
    await expect(
      git(source.repoDir, [
        "-c",
        `http.extraHeader=X-Ripgit-Actor-Name: ${intruder}`,
        "push",
        "ripgit",
        "HEAD:refs/heads/main",
      ]),
    ).rejects.toThrow(/403/);
  });
});

describe("organizations", () => {
  test("org owners and admins administer every org repo", async () => {
    const org = newName("org");
    const owner = newName("orgowner");
    const admin = newName("orgadmin");
    await seedOrg(org, { [owner]: "owner", [admin]: "member,admin" });

    const source = await push(org, "repo", actorHeaders(owner));
    expect(await settingsStatus(org, "repo", actorHeaders(owner))).toBe(200);
    expect(await settingsStatus(org, "repo", actorHeaders(admin))).toBe(200);
    expect(await pushesAs(source, actorHeaders(admin))).toBe(true);
  });

  test("members get the org default; outsiders see a private repo as missing", async () => {
    const org = newName("org");
    const owner = newName("orgowner");
    const member = newName("member");
    await seedOrg(org, { [owner]: "owner", [member]: "member" }, "read");

    const source = await push(org, "secret", {
      ...actorHeaders(owner),
      "X-Ripgit-Repo-Visibility": "private",
    });

    const asMember = await server.dispatch(`/${org}/secret/refs`, {
      headers: actorHeaders(member),
    });
    expect(asMember.status).toBe(200);
    await asMember.arrayBuffer();

    const asOutsider = await server.dispatch(`/${org}/secret/refs`, {
      headers: actorHeaders(newName("outsider")),
    });
    expect(asOutsider.status).toBe(404);
    await asOutsider.arrayBuffer();

    // read is not write
    expect(await pushesAs(source, actorHeaders(member))).toBe(false);
  });

  test("a team grant reaches the team's members", async () => {
    const org = newName("org");
    const owner = newName("orgowner");
    const dev = newName("dev");
    const orgId = await seedOrg(org, { [owner]: "owner", [dev]: "member" }, "none");
    const teamId = `team:${org}:devs`;
    await server.db
      .prepare(
        `INSERT INTO team (id, name, "memberCount", "organizationId", "createdAt") VALUES (?, 'devs', 1, ?, ?)`,
      )
      .bind(teamId, orgId, Date.now())
      .run();
    await server.db
      .prepare(`INSERT INTO "teamMember" (id, "teamId", "userId", "createdAt") VALUES (?, ?, ?, ?)`)
      .bind(`tm:${teamId}:${dev}`, teamId, userId(dev), Date.now())
      .run();

    const source = await push(org, "repo", actorHeaders(owner));
    expect(await pushesAs(source, actorHeaders(dev))).toBe(false);

    await grant(`${org}/repo`, "team", teamId, "write");
    expect(await pushesAs(source, actorHeaders(dev))).toBe(true);
  });
});

describe("mirror agents", () => {
  test("may push only to the repo their grant names", async () => {
    const owner = newName("owner");
    const source = await push(owner, "repo", actorHeaders(owner));
    await push(owner, "other", actorHeaders(owner));

    expect(await pushesAs(source, mirrorAgentHeaders(`${owner}/other`))).toBe(false);
    expect(await pushesAs(source, mirrorAgentHeaders(`${owner}/repo`))).toBe(true);
  });
});
