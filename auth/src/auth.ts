/**
 * better-auth configuration for the ripgit auth worker.
 *
 * Identity, sessions, organizations, teams, and API keys all live in the D1
 * database bound as AUTH_DB. ripgit binds the same database (as DIRECTORY)
 * and reads these tables to resolve roles, so the better-auth version is
 * pinned exactly in package.json: a schema change there is a ripgit change.
 *
 * `authOptions` is a pure function of its inputs so that
 * scripts/generate-schema.ts can build the schema from this exact config.
 */

import { apiKey } from "@better-auth/api-key";
import { betterAuth, type BetterAuthOptions } from "better-auth";
import { APIError } from "better-auth/api";
import { organization } from "better-auth/plugins";
import type { Env, Secret } from "./types";

/**
 * Owner names that can never be claimed, because the auth worker or ripgit
 * routes them itself. `/:owner/` shares the URL space with these.
 */
export const RESERVED_NAMES = new Set([
  "api",
  "login",
  "logout",
  "settings",
  "oidc",
  "orgs",
  "admin",
  "auth",
  "new",
]);

/** Owner names must be valid URL segments and never change once claimed. */
const NAME_PATTERN = /^[a-z0-9](?:[a-z0-9-]{0,37}[a-z0-9])?$/;

export function normalizeName(name: string): string {
  return name.trim().toLowerCase();
}

/**
 * Why `name` cannot be claimed as an owner namespace, or null if it can.
 * Checks shape and reservation only; availability is checked against D1.
 */
export function nameProblem(name: string): string | null {
  if (!NAME_PATTERN.test(name)) {
    return "names are 1-39 characters of a-z, 0-9, and inner hyphens";
  }
  if (RESERVED_NAMES.has(name)) return `"${name}" is reserved`;
  return null;
}

async function namespaceTaken(db: D1Database, name: string): Promise<boolean> {
  const row = await db
    .prepare("SELECT 1 FROM ripgit_namespaces WHERE name = ?")
    .bind(name)
    .first();
  return row !== null;
}

/** Comma-separated GitHub logins allowed to create organizations. */
function orgCreators(env: Pick<AuthConfig, "ORG_CREATORS">): Set<string> {
  return new Set(
    (env.ORG_CREATORS ?? "")
      .split(",")
      .map(normalizeName)
      .filter(Boolean),
  );
}

/** Everything authOptions needs, with secrets already read. */
export interface AuthConfig {
  BETTER_AUTH_URL: string;
  BETTER_AUTH_SECRET: string;
  GITHUB_CLIENT_ID: string;
  GITHUB_CLIENT_SECRET: string;
  ORG_CREATORS?: string;
  AUTH_DB: D1Database;
}

export function authOptions(
  env: AuthConfig,
  database: BetterAuthOptions["database"] = env.AUTH_DB,
) {
  const db = env.AUTH_DB;

  return {
    baseURL: env.BETTER_AUTH_URL,
    secret: env.BETTER_AUTH_SECRET,
    database,
    socialProviders: {
      github: {
        clientId: env.GITHUB_CLIENT_ID,
        clientSecret: env.GITHUB_CLIENT_SECRET,
        // Keep the GitHub login: it becomes the user's owner namespace.
        mapProfileToUser: (profile) => ({ login: normalizeName(profile.login) }),
      },
    },
    user: {
      additionalFields: {
        login: { type: "string", required: false, input: false },
      },
    },
    databaseHooks: {
      user: {
        create: {
          // Claim the user's namespace on first sign-in. If the name is
          // reserved or already taken (by an org, say), the account still
          // works; the user just owns no namespace until one is assigned.
          after: async (user) => {
            const login = (user as { login?: string | null }).login;
            if (!login || nameProblem(login)) return;
            await db
              .prepare(
                "INSERT OR IGNORE INTO ripgit_namespaces (name, kind, user_id, created_at) VALUES (?, 'user', ?, ?)",
              )
              .bind(login, user.id, Date.now())
              .run();
          },
        },
      },
    },
    plugins: [
      organization({
        // Teams are created deliberately, not one per org by default.
        teams: { enabled: true, defaultTeam: { enabled: false } },
        allowUserToCreateOrganization: (user) =>
          orgCreators(env).has(normalizeName(String(user.login ?? ""))),
        organizationHooks: {
          // Orgs share the owner namespace with users. Check before creating so
          // a refused name leaves nothing behind; claim after, once the org id
          // exists.
          beforeCreateOrganization: async ({ organization: org }) => {
            const slug = normalizeName(org.slug ?? "");
            const problem = nameProblem(slug);
            if (problem) throw new APIError("BAD_REQUEST", { message: problem });
            if (await namespaceTaken(db, slug)) {
              throw new APIError("BAD_REQUEST", {
                message: `"${slug}" is already taken`,
              });
            }
            return { data: { ...org, slug } };
          },
          afterCreateOrganization: async ({ organization: org }) => {
            await db
              .prepare(
                "INSERT INTO ripgit_namespaces (name, kind, org_id, created_at) VALUES (?, 'org', ?, ?)",
              )
              .bind(org.slug, org.id, Date.now())
              .run();
          },
          // Repos live in Durable Objects named "{owner}/{repo}" forever, so an
          // org's slug is its permanent address.
          beforeUpdateOrganization: async ({ organization: update }) => {
            if (update.slug !== undefined) {
              throw new APIError("BAD_REQUEST", {
                message: "an organization's slug cannot be changed",
              });
            }
          },
        },
      }),
      apiKey({
        defaultPrefix: "rg_",
        enableMetadata: true,
        // git makes several requests per clone or push; the plugin's default
        // (10 per day) would lock a key out within a couple of fetches.
        rateLimit: { enabled: false },
      }),
    ],
  } satisfies BetterAuthOptions;
}

/** The value of a secret, whether bound from the Secrets Store or as a string. */
export async function readSecret(name: string, secret: Secret | undefined): Promise<string> {
  if (typeof secret === "string") return secret;
  if (!secret) throw new Error(`${name} is not bound`);
  try {
    return await secret.get();
  } catch (err) {
    throw new Error(`${name} could not be read from the Secrets Store: ${String(err)}`);
  }
}

export async function createAuth(env: Env) {
  const [clientId, clientSecret, authSecret] = await Promise.all([
    readSecret("GITHUB_CLIENT_ID", env.GITHUB_CLIENT_ID),
    readSecret("GITHUB_CLIENT_SECRET", env.GITHUB_CLIENT_SECRET),
    readSecret("BETTER_AUTH_SECRET", env.BETTER_AUTH_SECRET),
  ]);
  return betterAuth(
    authOptions({
      BETTER_AUTH_URL: env.BETTER_AUTH_URL,
      BETTER_AUTH_SECRET: authSecret,
      GITHUB_CLIENT_ID: clientId,
      GITHUB_CLIENT_SECRET: clientSecret,
      ORG_CREATORS: env.ORG_CREATORS,
      AUTH_DB: env.AUTH_DB,
    }),
  );
}

export type Auth = Awaited<ReturnType<typeof createAuth>>;
