/**
 * Print the SQL that creates better-auth's tables for this worker's exact
 * config (plugins and additional fields included).
 *
 *   node scripts/generate-schema.ts > migrations/NNNN_better_auth.sql
 *
 * Runs better-auth's own migration planner against an empty in-memory
 * SQLite, so the output is the full schema. D1 is SQLite, so it applies as-is.
 * Re-run after changing auth.ts or bumping better-auth, and diff the result
 * into a new migration.
 */

import { DatabaseSync } from "node:sqlite";
import { getMigrations } from "better-auth/db/migration";
import { authOptions } from "../src/auth.ts";

const options = authOptions(
  {
    BETTER_AUTH_URL: "http://localhost",
    BETTER_AUTH_SECRET: "schema-generation-only-not-a-real-secret",
    GITHUB_CLIENT_ID: "unused",
    GITHUB_CLIENT_SECRET: "unused",
    ORG_CREATORS: "",
    AUTH_DB: undefined as unknown as D1Database,
  },
  new DatabaseSync(":memory:"),
);

const { compileMigrations } = await getMigrations(options);
process.stdout.write((await compileMigrations()).trim() + "\n");
