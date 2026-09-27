import type { OAuthHelpers } from "@cloudflare/workers-oauth-provider";

// ---------------------------------------------------------------------------
// Actor — identity passed from auth worker to ripgit via trusted headers
// ---------------------------------------------------------------------------

export interface ActorProps {
  actorId: string;          // stable: "github:12345" | "agent:uuid"
  actorName: string;        // display: github username for users, token name for agents
  actorKind: "user" | "agent";
  ownerActorId?: string;    // agents only: owning user's actorId
  ownerActorName?: string;  // agents only: owning user's GitHub username (used for ownership checks)
  scopes: string[];
  // Mirror agents only: the single "owner/repo" path this credential may touch.
  // Enforced on every proxied request, since ripgit's own check is owner-wide.
  repoScope?: string;
  // Mirror agents only: the upstream's visibility, taken from the signed OIDC
  // claim. Tells ripgit whether the mirror it is about to receive is public.
  repoVisibility?: "public" | "private";
}

// ---------------------------------------------------------------------------
// Env — Cloudflare Worker bindings
// ---------------------------------------------------------------------------

export interface Env {
  OAUTH_KV: KVNamespace;
  RIPGIT: Fetcher;
  GITHUB_CLIENT_ID: string;
  GITHUB_CLIENT_SECRET: string;
  // Random secret for signing session cookies — set with: wrangler secret put SESSION_SECRET
  SESSION_SECRET: string;
  // Audience that GitHub Actions OIDC tokens must carry. Pin to this
  // deployment's URL so tokens minted for other services cannot be replayed.
  OIDC_AUDIENCE: string;
  // Injected by workers-oauth-provider for defaultHandler and apiHandler calls
  OAUTH_PROVIDER: OAuthHelpers;
}
