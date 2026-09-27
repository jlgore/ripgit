import { randomUUID } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Miniflare } from "miniflare";

const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const scriptPath = resolve(rootDir, "build/index.js");
const migrationsDir = resolve(rootDir, "auth/migrations");

// Stands in for the auth worker in front of ripgit. Like a first GitHub
// sign-in, the first request from a user claims their namespace, so tests can
// keep sending plain X-Ripgit-Actor-Name headers. Mirror agents are not users
// and claim nothing. It deliberately does not strip X-Ripgit-* headers: ripgit
// must not trust a role header that arrives from outside, and tests check that.
const gatewayScript = `
export default {
  async fetch(request, env) {
    const name = request.headers.get("X-Ripgit-Actor-Name");
    const kind = request.headers.get("X-Ripgit-Actor-Kind") ?? "user";
    if (name && kind !== "mirror") {
      const headers = new Headers(request.headers);
      let id = headers.get("X-Ripgit-Actor-Id");
      if (!id) {
        id = "test-user:" + name.toLowerCase();
        headers.set("X-Ripgit-Actor-Id", id);
      }
      await env.DIRECTORY.prepare(
        "INSERT OR IGNORE INTO ripgit_namespaces (name, kind, user_id, created_at) VALUES (?, 'user', ?, ?)",
      ).bind(name.toLowerCase(), id, Date.now()).run();
      request = new Request(request, { headers });
    }
    return env.RIPGIT.fetch(request);
  },
};
`;

/** Every statement in auth/migrations, in order, as the deployment applies them. */
function migrationStatements() {
  return readdirSync(migrationsDir)
    .filter((f) => f.endsWith(".sql"))
    .sort()
    .flatMap((f) =>
      readFileSync(resolve(migrationsDir, f), "utf8")
        .split("\n")
        .filter((line) => !line.trimStart().startsWith("--"))
        .join("\n")
        .split(";")
        .map((stmt) => stmt.trim())
        .filter(Boolean),
    );
}

function miniflareOptions() {
  return {
    workers: [
      {
        name: "gateway",
        modules: true,
        script: gatewayScript,
        compatibilityDate: "2026-03-18",
        d1Databases: { DIRECTORY: "directory" },
        serviceBindings: { RIPGIT: "ripgit" },
      },
      {
        name: "ripgit",
        scriptPath,
        compatibilityDate: "2026-03-18",
        modules: true,
        modulesRules: [
          { type: "CompiledWasm", include: ["**/*.wasm"], fallthrough: true },
        ],
        kvNamespaces: ["REGISTRY"],
        d1Databases: { DIRECTORY: "directory" },
        // Credential the GitHub pull mirror authenticates with. The upstream in
        // tests is another ripgit repo, which ignores Authorization entirely —
        // but it must be set for the sync path to run at all.
        bindings: { GITHUB_MIRROR_TOKEN: "test-mirror-token" },
        durableObjects: {
          REPOSITORY: {
            className: "Repository",
            useSQLite: true,
          },
        },
      },
    ],
  };
}

export async function createTestServer() {
  const mf = new Miniflare(miniflareOptions());
  const url = await mf.ready;
  const db = await mf.getD1Database("DIRECTORY", "ripgit");
  await db.batch(migrationStatements().map((sql) => db.prepare(sql)));

  return {
    mf,
    url,
    /** The DIRECTORY D1 database, for seeding orgs, teams, and grants. */
    db,
    dispatch(path = "/", init) {
      return mf.dispatchFetch(new URL(path, url).toString(), init);
    },
  };
}

/** Headers the auth worker sends for a signed-in user. */
export function actorHeaders(actorName, headers = {}) {
  return {
    "X-Ripgit-Actor-Name": actorName,
    ...headers,
  };
}

/** Headers the auth worker sends for an OIDC mirror agent scoped to `repo`. */
export function mirrorAgentHeaders(repo, headers = {}) {
  return {
    "X-Ripgit-Actor-Name": repo.split("/")[0],
    "X-Ripgit-Actor-Kind": "mirror",
    "X-Ripgit-Actor-Repo": repo,
    "X-Ripgit-Actor-Scopes": "push,mirror",
    ...headers,
  };
}

export function ownerHeaders(owner, headers = {}) {
  return actorHeaders(owner, headers);
}

export function uniqueId(prefix) {
  return `${prefix}-${randomUUID().slice(0, 8)}`;
}
