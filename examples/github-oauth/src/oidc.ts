// ---------------------------------------------------------------------------
// GitHub Actions OIDC — keyless authentication for mirroring
// ---------------------------------------------------------------------------
//
// A workflow with `permissions: id-token: write` can ask Actions for a signed
// JWT describing itself, and exchange it here for a short-lived ripgit token.
// Nothing long-lived is ever stored in GitHub secrets, and a leaked token is
// worthless within minutes.
//
// Trust chain: the JWT is signed by GitHub, verified against GitHub's published
// JWKS, and its claims say which repository and ref produced it. Everything the
// caller asserts about itself is signed; nothing is taken on trust from the
// request body.

const GITHUB_ISSUER = "https://token.actions.githubusercontent.com";
const JWKS_URL = `${GITHUB_ISSUER}/.well-known/jwks`;
const JWKS_CACHE_KEY = "jwks:github-actions";
const JWKS_CACHE_TTL = 3600;

/** Claims we rely on. GitHub sets many more; these are the load-bearing ones. */
export interface GitHubOidcClaims {
  iss: string;
  aud: string | string[];
  exp: number;
  nbf?: number;
  jti?: string;
  /** "owner/repo" of the repository running the workflow. */
  repository: string;
  /** Owning user or org — differs from the repo owner only in casing. */
  repository_owner: string;
  /** Full ref that triggered the run, e.g. "refs/heads/main". */
  ref?: string;
  /** Commit SHA that triggered the run. */
  sha?: string;
  /** GitHub login that triggered the run — for audit, never for authorization. */
  actor?: string;
  /** Subject, e.g. "repo:owner/repo:ref:refs/heads/main". */
  sub?: string;
}

export class OidcError extends Error {}

// ---------------------------------------------------------------------------
// base64url
// ---------------------------------------------------------------------------

function base64UrlToBytes(input: string): Uint8Array {
  const padded = input.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(padded + "=".repeat((4 - (padded.length % 4)) % 4));
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

function base64UrlToString(input: string): string {
  return new TextDecoder().decode(base64UrlToBytes(input));
}

// ---------------------------------------------------------------------------
// JWKS
// ---------------------------------------------------------------------------

interface Jwks {
  keys: (JsonWebKey & { kid: string })[];
}

/**
 * Fetch GitHub's signing keys, cached in KV.
 *
 * `force` bypasses the cache — used once when a token names a `kid` we have not
 * seen, so that key rotation self-heals instead of failing until the TTL lapses.
 */
async function fetchJwks(kv: KVNamespace, force: boolean): Promise<Jwks> {
  if (!force) {
    const cached = await kv.get(JWKS_CACHE_KEY);
    if (cached) return JSON.parse(cached) as Jwks;
  }

  const resp = await fetch(JWKS_URL, {
    headers: { Accept: "application/json" },
  });
  if (!resp.ok) {
    throw new OidcError(`could not fetch GitHub JWKS: HTTP ${resp.status}`);
  }
  const jwks = (await resp.json()) as Jwks;
  await kv.put(JWKS_CACHE_KEY, JSON.stringify(jwks), {
    expirationTtl: JWKS_CACHE_TTL,
  });
  return jwks;
}

async function findKey(
  kv: KVNamespace,
  kid: string,
): Promise<JsonWebKey> {
  for (const force of [false, true]) {
    const jwks = await fetchJwks(kv, force);
    const key = jwks.keys.find((k) => k.kid === kid);
    if (key) return key;
  }
  throw new OidcError(`no GitHub signing key matches kid ${kid}`);
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/**
 * Verify a GitHub Actions OIDC token and return its claims.
 *
 * `expectedAudience` must be pinned to this deployment. GitHub's default
 * audience is the repository owner's URL, which is shared with every other
 * service that owner runs — a token minted for one of them would otherwise be
 * replayable here.
 */
export async function verifyGitHubOidcToken(
  jwt: string,
  expectedAudience: string,
  kv: KVNamespace,
): Promise<GitHubOidcClaims> {
  const parts = jwt.split(".");
  if (parts.length !== 3) throw new OidcError("malformed JWT");
  const [rawHeader, rawPayload, rawSignature] = parts;

  let header: { alg?: string; kid?: string };
  let claims: GitHubOidcClaims;
  try {
    header = JSON.parse(base64UrlToString(rawHeader));
    claims = JSON.parse(base64UrlToString(rawPayload)) as GitHubOidcClaims;
  } catch {
    throw new OidcError("JWT header or payload is not valid JSON");
  }

  if (header.alg !== "RS256") {
    throw new OidcError(`unexpected JWT algorithm ${header.alg}`);
  }
  if (!header.kid) throw new OidcError("JWT has no kid");

  // Verify the signature before trusting any claim.
  const jwk = await findKey(kv, header.kid);
  const key = await crypto.subtle.importKey(
    "jwk",
    jwk,
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["verify"],
  );
  const signed = new TextEncoder().encode(`${rawHeader}.${rawPayload}`);
  const valid = await crypto.subtle.verify(
    "RSASSA-PKCS1-v1_5",
    key,
    base64UrlToBytes(rawSignature),
    signed,
  );
  if (!valid) throw new OidcError("JWT signature verification failed");

  if (claims.iss !== GITHUB_ISSUER) {
    throw new OidcError(`unexpected issuer ${claims.iss}`);
  }

  const audiences = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
  if (!audiences.includes(expectedAudience)) {
    throw new OidcError("JWT audience does not match this deployment");
  }

  const now = Math.floor(Date.now() / 1000);
  if (typeof claims.exp !== "number" || claims.exp <= now) {
    throw new OidcError("JWT has expired");
  }
  if (typeof claims.nbf === "number" && claims.nbf > now + 60) {
    throw new OidcError("JWT is not yet valid");
  }
  if (!claims.repository) throw new OidcError("JWT has no repository claim");

  // Single-use within the token's validity window.
  if (claims.jti) {
    const seenKey = `oidc-jti:${claims.jti}`;
    if (await kv.get(seenKey)) {
      throw new OidcError("JWT has already been exchanged");
    }
    await kv.put(seenKey, "1", {
      expirationTtl: Math.max(60, claims.exp - now),
    });
  }

  return claims;
}

// ---------------------------------------------------------------------------
// Mirror allowlist
// ---------------------------------------------------------------------------

/**
 * Which GitHub repo may mirror into which ripgit repo.
 *
 * Two levels, most specific first:
 *
 *   `mirror:<owner>/<repo>`  a single repo, with an explicit target and refs
 *   `mirror-owner:<owner>`   any repo under that owner, mirroring to the
 *                            matching `<owner>/<repo>` path in ripgit
 *
 * Owner-level trust is safe because the minted token stays scoped to the one
 * repository the OIDC claim names: a workflow can only ever write to its own
 * mirror, which is the only thing it would legitimately do. What owner-level
 * enrollment removes is the bookkeeping -- adding `mirror.yml` to a repo is
 * then the only step, rather than a repo change plus a KV write.
 *
 * Stored as JSON:
 *
 *   { "target": "jlgore/ripgit", "refs": ["refs/heads/main"] }   (per repo)
 *   { "refs": ["refs/heads/main"] }                              (per owner)
 */
export interface MirrorGrant {
  /** "owner/repo" path within ripgit that this workflow may push to. */
  target: string;
  /** If set, only these refs may trigger an exchange. */
  refs?: string[];
}

export async function lookupMirrorGrant(
  kv: KVNamespace,
  repository: string,
): Promise<MirrorGrant | null> {
  const key = repository.toLowerCase();

  const explicit = await kv.get(`mirror:${key}`);
  if (explicit) {
    const grant = JSON.parse(explicit) as MirrorGrant;
    if (!grant.target || !grant.target.includes("/")) {
      throw new OidcError(`mirror grant for ${repository} has no valid target`);
    }
    return grant;
  }

  const [owner, name] = key.split("/");
  if (!owner || !name) return null;

  const byOwner = await kv.get(`mirror-owner:${owner}`);
  if (byOwner) {
    // The target is derived, never taken from the request: the owner comes from
    // the signed claim and the repo name with it.
    const grant = JSON.parse(byOwner) as Omit<MirrorGrant, "target">;
    return { target: `${owner}/${name}`, refs: grant.refs };
  }

  return null;
}
