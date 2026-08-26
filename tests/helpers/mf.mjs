import { randomUUID } from "node:crypto";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Miniflare } from "miniflare";

const rootDir = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const scriptPath = resolve(rootDir, "build/index.js");

function miniflareOptions() {
  return {
    workers: [
      {
        name: "ripgit",
        scriptPath,
        compatibilityDate: "2026-03-18",
        modules: true,
        modulesRules: [
          { type: "CompiledWasm", include: ["**/*.wasm"], fallthrough: true },
        ],
        kvNamespaces: ["REGISTRY"],
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

  return {
    mf,
    url,
    dispatch(path = "/", init) {
      return mf.dispatchFetch(new URL(path, url).toString(), init);
    },
  };
}

export function actorHeaders(actorName, headers = {}) {
  return {
    "X-Ripgit-Actor-Name": actorName,
    ...headers,
  };
}

export function ownerHeaders(owner, headers = {}) {
  return actorHeaders(owner, headers);
}

export function uniqueId(prefix) {
  return `${prefix}-${randomUUID().slice(0, 8)}`;
}
