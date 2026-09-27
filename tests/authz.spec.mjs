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

function form(fields) {
  return {
    "Content-Type": "application/x-www-form-urlencoded",
    body: new URLSearchParams(fields).toString(),
  };
}

async function postSettings(owner, repo, action, fields, headers) {
  const { body, ...contentType } = form(fields);
  const resp = await server.dispatch(`/${owner}/${repo}/settings/${action}`, {
    method: "POST",
    headers: { ...headers, ...contentType },
    body,
    redirect: "manual",
  });
  const text = await resp.text();
  return { status: resp.status, text };
}

describe("internal visibility", () => {
  test("org members can read an internal repo; outsiders and anonymous cannot", async () => {
    const org = newName("org");
    const owner = newName("orgowner");
    const member = newName("member");
    await seedOrg(org, { [owner]: "owner", [member]: "member" }, "none");
    await push(org, "inside", actorHeaders(owner));

    const set = await postSettings(org, "inside", "visibility", { visibility: "internal" }, actorHeaders(owner));
    expect(set.status).toBe(302);

    const read = async (headers) => {
      const resp = await server.dispatch(`/${org}/inside/refs`, { headers });
      await resp.arrayBuffer();
      return resp.status;
    };
    // default_repo_role is "none", so this is visibility alone at work.
    expect(await read(actorHeaders(member))).toBe(200);
    expect(await read(actorHeaders(newName("outsider")))).toBe(404);
    expect(await read({})).toBe(404);

    const listing = async (headers) =>
      (await server.dispatch(`/${org}/`, { headers: { ...headers, Accept: "text/markdown" } })).text();
    expect(await listing(actorHeaders(member))).toContain("inside");
    expect(await listing(actorHeaders(newName("outsider")))).not.toContain("inside");
  });

  test("a user namespace cannot choose internal", async () => {
    const owner = newName("owner");
    await push(owner, "repo", actorHeaders(owner));
    const set = await postSettings(owner, "repo", "visibility", { visibility: "internal" }, actorHeaders(owner));
    expect(set.status).toBe(400);
  });

  test("unknown visibility values are refused, not coerced", async () => {
    const owner = newName("owner");
    await push(owner, "repo", actorHeaders(owner));
    const set = await postSettings(owner, "repo", "visibility", { visibility: "secret" }, actorHeaders(owner));
    expect(set.status).toBe(400);
  });
});

describe("repo settings: grants", () => {
  test("an admin grants and revokes a user's role by login", async () => {
    const owner = newName("owner");
    const helper = newName("helper");
    await seedUser(helper);
    const source = await push(owner, "repo", actorHeaders(owner));

    const granted = await postSettings(
      owner, "repo", "grant",
      { kind: "user", grantee: helper, role: "write" },
      actorHeaders(owner),
    );
    expect(granted.status).toBe(302);
    expect(await pushesAs(source, actorHeaders(helper))).toBe(true);

    const page = await server.dispatch(`/${owner}/repo/settings`, {
      headers: actorHeaders(owner, { Accept: "text/markdown" }),
    });
    expect(await page.text()).toContain(`\`${helper}\``);

    const revoked = await postSettings(
      owner, "repo", "revoke",
      { kind: "user", id: userId(helper) },
      actorHeaders(owner),
    );
    expect(revoked.status).toBe(302);
    expect(await pushesAs(source, actorHeaders(helper))).toBe(false);
  });

  test("granting to someone who has never signed in is a 400", async () => {
    const owner = newName("owner");
    await push(owner, "repo", actorHeaders(owner));
    const resp = await postSettings(
      owner, "repo", "grant",
      { kind: "user", grantee: newName("ghost"), role: "read" },
      actorHeaders(owner),
    );
    expect(resp.status).toBe(400);
    expect(resp.text).toContain("has signed in");
  });

  test("only admins may change grants", async () => {
    const owner = newName("owner");
    const writer = newName("writer");
    await push(owner, "repo", actorHeaders(owner));
    await grant(`${owner}/repo`, "user", userId(writer), "write");
    const resp = await postSettings(
      owner, "repo", "grant",
      { kind: "user", grantee: writer, role: "admin" },
      actorHeaders(writer),
    );
    expect(resp.status).toBe(403);
  });
});

describe("role-aware pages", () => {
  test("org admins see the Settings tab on org repos; members do not", async () => {
    const org = newName("org");
    const admin = newName("orgadmin");
    const member = newName("member");
    await seedOrg(org, { [admin]: "admin", [member]: "member" }, "read");
    await push(org, "repo", actorHeaders(admin));

    const settingsLink = `/${org}/repo/settings`;
    const page = async (who) =>
      (await server.dispatch(`/${org}/repo/`, { headers: actorHeaders(who, { Accept: "text/html" }) })).text();
    expect(await page(admin)).toContain(settingsLink);
    expect(await page(member)).not.toContain(settingsLink);
  });
});
