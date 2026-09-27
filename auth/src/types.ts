// ---------------------------------------------------------------------------
// Actor — identity passed from auth worker to ripgit via trusted headers
// ---------------------------------------------------------------------------

export interface Actor {
  /** better-auth user.id — what ripgit authorizes on. Empty for mirror agents. */
  userId: string;
  /** The user's owner namespace (lowercased GitHub login), or for mirror
   *  agents the owner of the repo they may push to. */
  login: string;
  /** "user": browser session. "agent": API key. "mirror": OIDC-minted token. */
  kind: "user" | "agent" | "mirror";
  /** API key name, for display and audit. */
  keyName?: string;
  scopes: string[];
  // Mirror agents only: the single "owner/repo" path this credential may touch.
  repoScope?: string;
  // Mirror agents only: the upstream's visibility, taken from the signed OIDC
  // claim. Tells ripgit whether the mirror it is about to receive is public.
  repoVisibility?: "public" | "private";
}

// ---------------------------------------------------------------------------
// Env — Cloudflare Worker bindings
// ---------------------------------------------------------------------------

export interface Env {
  /** better-auth tables plus ripgit_* tables; shared with ripgit as DIRECTORY. */
  AUTH_DB: D1Database;
  /** GitHub OIDC JWKS cache, mirror enrollment, and short-lived mirror tokens. */
  OAUTH_KV: KVNamespace;
  RIPGIT: Fetcher;
  GITHUB_CLIENT_ID: string;
  GITHUB_CLIENT_SECRET: string;
  /** Public origin of this worker, e.g. https://ripgit-auth.example.workers.dev */
  BETTER_AUTH_URL: string;
  /** wrangler secret put BETTER_AUTH_SECRET (32+ random bytes) */
  BETTER_AUTH_SECRET: string;
  /** Comma-separated GitHub logins allowed to create organizations. */
  ORG_CREATORS?: string;
  // Audience that GitHub Actions OIDC tokens must carry. Pin to this
  // deployment's URL so tokens minted for other services cannot be replayed.
  OIDC_AUDIENCE: string;
}
