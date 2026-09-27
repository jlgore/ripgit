/**
 * ripgit auth worker
 *
 * Identity comes from better-auth (see ./auth.ts): GitHub sign-in, browser
 * sessions, organizations and teams, and API keys, all stored in D1. This
 * worker resolves who the caller is, then forwards to ripgit with trusted
 * X-Ripgit-Actor-* headers. ripgit decides what that caller may do.
 *
 * Routes handled here (everything else forwarded to ripgit):
 *   *    /api/auth/*     → better-auth (OAuth callback, sessions, orgs, API keys)
 *   GET  /               → landing page (HTML or text mode)
 *   GET  /login          → start GitHub sign-in, ?next= redirect after
 *   GET  /logout         → end the session
 *   GET  /settings       → API key management (HTML or text mode, requires login)
 *   POST /settings/tokens                → create an API key
 *   POST /settings/tokens/:id/revoke     → revoke an API key
 *   POST /oidc/github/exchange           → GitHub Actions OIDC → mirror token
 *
 * Setup: see auth/README.md.
 */

import { createAuth, type Auth } from "./auth";
import type { Actor, Env } from "./types";
import {
  lookupMirrorGrant,
  OidcError,
  verifyGitHubOidcToken,
} from "./oidc";

/** Scopes carried by a signed-in user or their API keys. */
const USER_SCOPES = ["repo:read", "repo:write", "issue:write", "pr:merge"];

type PageFormat = "html" | "markdown" | "text";

interface PageFormatSelection {
  format: PageFormat;
  varyAccept: boolean;
}

interface TextAction {
  method: "GET" | "POST";
  path: string;
  description: string;
  requires?: string;
  fields?: string[];
  effect?: string;
}

// One better-auth instance per isolate: env is stable for an isolate's life.
const authInstances = new WeakMap<Env, Auth>();

function getAuth(env: Env): Auth {
  let auth = authInstances.get(env);
  if (!auth) {
    auth = createAuth(env);
    authInstances.set(env, auth);
  }
  return auth;
}

export default {
  fetch: mainHandler,
} satisfies ExportedHandler<Env>;

// ---------------------------------------------------------------------------
// mainHandler — resolves actor first, routes, then forwards to ripgit
// ---------------------------------------------------------------------------

async function mainHandler(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const auth = getAuth(env);

  if (url.pathname.startsWith("/api/auth/")) return auth.handler(request);

  const pageFormat = preferredPageFormat(request);

  // Resolve identity up front — available to all routes below
  const actor = await resolveActor(request, env, auth);

  // ── Auth + settings routes ────────────────────────────────────────────────

  if (url.pathname === "/login") return handleLogin(request, auth);
  if (url.pathname === "/logout") return handleLogout(request, auth);
  if (url.pathname === "/oidc/github/exchange" && request.method === "POST") {
    return handleOidcExchange(request, env);
  }

  if (url.pathname === "/settings") {
    if (!actor || actor.kind === "mirror") {
      if (pageFormat.format === "html") return redirect(`/login?next=/settings`);
      return renderSettingsAuthRequiredPage(pageFormat);
    }
    return handleSettings(request, auth, actor, undefined, pageFormat);
  }
  if (url.pathname === "/settings/tokens" && request.method === "POST") {
    if (!actor || actor.kind !== "user") return redirect(`/login?next=/settings`);
    return handleCreateToken(request, auth, actor);
  }
  // /settings/tokens/:keyId/revoke
  const revokeMatch = url.pathname.match(
    /^\/settings\/tokens\/([^/]+)\/revoke$/,
  );
  if (revokeMatch && request.method === "POST") {
    if (!actor || actor.kind !== "user") return redirect(`/login?next=/settings`);
    return handleRevokeToken(
      request,
      auth,
      decodeURIComponent(revokeMatch[1]),
    );
  }

  if (url.pathname === "/" && request.method === "GET") {
    // Logged-in users go straight to their profile page
    if (actor?.login && actor.kind === "user" && pageFormat.format === "html") {
      return redirect(`/${actor.login}/`);
    }
    return renderLandingPage(new URL(request.url).origin, actor, pageFormat);
  }

  // ── Everything else → ripgit ──────────────────────────────────────────────
  return forwardToRipgit(request, actor, env);
}

function preferredPageFormat(request: Request): PageFormatSelection {
  const url = new URL(request.url);
  const format = url.searchParams.get("format")?.trim().toLowerCase();

  if (format === "html") return { format: "html", varyAccept: false };
  if (format === "md" || format === "markdown") {
    return { format: "markdown", varyAccept: false };
  }
  if (format === "text" || format === "txt" || format === "plain") {
    return { format: "text", varyAccept: false };
  }

  const accept = request.headers.get("Accept") ?? "";
  if (accept.includes("text/markdown")) {
    return { format: "markdown", varyAccept: true };
  }
  if (accept.includes("text/plain")) {
    return { format: "text", varyAccept: true };
  }
  if (accept.includes("text/html")) {
    return { format: "html", varyAccept: true };
  }

  return { format: "html", varyAccept: false };
}

function respondPage(
  body: string,
  selection: PageFormatSelection,
  status = 200,
): Response {
  const headers = new Headers();

  if (selection.format === "html") {
    headers.set("Content-Type", "text/html; charset=utf-8");
  } else {
    headers.set(
      "Content-Type",
      selection.format === "markdown"
        ? "text/markdown; charset=utf-8"
        : "text/plain; charset=utf-8",
    );
    headers.set("Cache-Control", "no-cache");
  }

  if (selection.varyAccept) {
    headers.set("Vary", "Accept");
  }

  return new Response(body, { status, headers });
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function textNavigationHint(selection: PageFormatSelection): string {
  const accept =
    selection.format === "markdown" ? "text/markdown" : "text/plain";
  const format = selection.format === "markdown" ? "md" : "text";
  return `GET paths below omit \`?format\`. Keep \`Accept: ${accept}\` to stay in text mode, or append \`?format=${format}\` when following a path without headers.`;
}

function renderTextActions(actions: TextAction[]): string {
  if (actions.length === 0) return "";

  const lines = ["", "## Actions"];
  for (const action of actions) {
    let line = `- ${action.method} \`${action.path}\` - ${action.description}`;
    if (action.fields?.length) {
      line += `; fields: ${action.fields.join(", ")}`;
    }
    if (action.requires) {
      line += `; requires ${action.requires}`;
    }
    if (action.effect) {
      line += `; ${action.effect}`;
    }
    lines.push(line);
  }
  return `${lines.join("\n")}\n`;
}

function renderTextHints(hints: string[]): string {
  if (hints.length === 0) return "";
  return `\n## Hints\n${hints.map((hint) => `- ${hint}`).join("\n")}\n`;
}

function authFooterHtml(): string {
  return `ripgit &mdash; <a href="https://github.com/deathbyknowledge/ripgit">open source</a> by <a href="https://x.com/caise_p">deathbyknowledge</a>`;
}

function renderAuthPageHtml(options: {
  title: string;
  topbarRight: string;
  content: string;
  mainClass?: string;
  footer?: string;
}): string {
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escapeHtml(options.title)} — ripgit</title>
<style>
  *{margin:0;padding:0;box-sizing:border-box}
  body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#fff;color:#1f2328;min-height:100vh;display:flex;flex-direction:column}
  a{color:#0969da;text-decoration:none}
  a:hover{text-decoration:underline}
  .site-header{border-bottom:1px solid #d1d9e0;background:#fff}
  .site-header-row{min-height:52px;padding:0 24px;display:flex;align-items:center;justify-content:space-between;gap:16px}
  .brand{font-weight:700;font-size:16px;color:#1f2328;text-decoration:none}
  .site-nav{display:flex;align-items:center;gap:12px;font-size:13px;color:#656d76;flex-wrap:wrap;justify-content:flex-end}
  .site-nav a{color:#656d76}
  .site-nav a:hover{color:#0969da}
  .site-nav strong{color:#1f2328}
  .site-shell{max-width:960px;margin:0 auto;width:100%;padding:40px 24px 56px}
  .landing-shell{flex:1;display:flex;flex-direction:column;justify-content:center;align-items:center;max-width:1040px;margin:0 auto;padding:72px 24px;text-align:center;width:100%}
  .hero{max-width:720px}
  .hero h1{font-size:40px;font-weight:700;letter-spacing:-1px;margin-bottom:16px}
  .eyebrow{font-size:13px;font-weight:600;letter-spacing:.08em;text-transform:uppercase;color:#656d76;margin-bottom:12px}
  .tagline{font-size:18px;color:#656d76;line-height:1.5;margin-bottom:20px}
  .hero-copy{font-size:15px;line-height:1.65;color:#3d444d;margin-bottom:28px}
  .cta-row{display:flex;gap:12px;justify-content:center;flex-wrap:wrap;margin-bottom:56px}
  .signin-btn,.btn-secondary{display:inline-flex;align-items:center;gap:10px;border-radius:6px;padding:12px 20px;font-size:15px;font-weight:600;text-decoration:none}
  .signin-btn{background:#1f2328;color:#fff}
  .signin-btn:hover{background:#393f47;color:#fff;text-decoration:none}
  .signin-btn svg{width:20px;height:20px;fill:#fff}
  .btn-secondary{background:#f6f8fa;border:1px solid #d1d9e0;color:#1f2328}
  .btn-secondary:hover{background:#eef2f6;text-decoration:none}
  .feature-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:18px;width:100%;max-width:900px;text-align:left}
  .feature-card{border:1px solid #d1d9e0;border-radius:8px;padding:18px;background:#fff}
  .feature-card h3{font-size:14px;font-weight:600;margin-bottom:6px}
  .feature-card p{font-size:13px;color:#656d76;line-height:1.6}
  .site-footer{padding:24px;border-top:1px solid #d1d9e0;text-align:center;font-size:12px;color:#656d76}
  .site-footer a{color:#656d76}
  h1{font-size:28px;margin-bottom:6px}
  h2{font-size:16px;margin:28px 0 10px;font-weight:600}
  p{line-height:1.6}
  .lede{color:#656d76;max-width:700px;margin-bottom:28px}
  .section{margin-top:28px}
  .banner{border:1px solid #d1d9e0;border-radius:8px;padding:16px;margin-bottom:24px}
  .banner-success{background:#dafbe1;border-color:#82cfac}
  .token-value{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:13px;background:#fff;border:1px solid #d1d9e0;border-radius:6px;padding:8px 10px;word-break:break-all;margin:10px 0;user-select:all}
  .cmd{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px;background:#f6f8fa;border:1px solid #d1d9e0;border-radius:6px;padding:12px 14px;margin:10px 0;overflow-x:auto;white-space:pre;line-height:1.6}
  .form-row{display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:6px}
  input[type=text]{border:1px solid #d1d9e0;border-radius:6px;padding:8px 12px;font-size:14px;min-width:280px;max-width:100%}
  input[type=text]:focus{outline:none;border-color:#0969da;box-shadow:0 0 0 3px rgba(9,105,218,.1)}
  .btn{background:#1f883d;color:#fff;border:none;border-radius:6px;padding:8px 16px;cursor:pointer;font-size:14px}
  .btn:hover{background:#1a7f37}
  .btn-danger{background:#cf222e;color:#fff;border:none;border-radius:6px;cursor:pointer}
  .btn-sm{padding:6px 12px;font-size:13px}
  .btn-danger:hover{background:#a40e26}
  table{width:100%;border-collapse:collapse;margin-top:8px}
  td,th{text-align:left;padding:10px 12px;border-bottom:1px solid #d1d9e0;font-size:14px;vertical-align:top}
  th{font-weight:600;background:#f6f8fa}
  .actions{text-align:right}
  .muted{color:#656d76;font-size:13px}
  code{background:#f6f8fa;border:1px solid #d1d9e0;border-radius:4px;padding:1px 5px;font-size:12px;font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
  @media (max-width: 720px){
    .site-header-row{padding:12px 20px;align-items:flex-start}
    .landing-shell{padding:56px 20px}
    .site-shell{padding:32px 20px 48px}
    .hero h1{font-size:34px}
    .tagline{font-size:17px}
  }
</style>
</head>
<body>
  <header class="site-header">
    <div class="site-header-row">
      <a href="/" class="brand">ripgit</a>
      <div class="site-nav">${options.topbarRight}</div>
    </div>
  </header>
  <main class="${options.mainClass ?? "site-shell"}">${options.content}</main>
  ${options.footer ? `<footer class="site-footer">${options.footer}</footer>` : ""}
</body>
</html>`;
}


// ---------------------------------------------------------------------------
// Settings — browser UI for API key management
// ---------------------------------------------------------------------------

interface TokenRow {
  agentId: string;
  name: string;
}

async function listTokens(request: Request, auth: Auth): Promise<TokenRow[]> {
  const result = await auth.api.listApiKeys({ headers: request.headers });
  const keys = Array.isArray(result) ? result : result.apiKeys;
  return keys.map((k) => ({ agentId: k.id, name: k.name ?? k.start ?? k.id }));
}

async function handleSettings(
  request: Request,
  auth: Auth,
  actor: Actor,
  newToken?: string,
  pageFormat = preferredPageFormat(request),
): Promise<Response> {
  // API-key callers may view settings; listing needs the owner's session, so
  // they see the page without the key list.
  const tokens = actor.kind === "user" ? await listTokens(request, auth) : [];
  const origin = new URL(request.url).origin;
  if (pageFormat.format === "html") {
    return respondPage(
      renderSettingsPageHtml(actor.login, tokens, origin, newToken),
      pageFormat,
    );
  }
  return respondPage(
    renderSettingsPageText(actor.login, tokens, origin, newToken, pageFormat),
    pageFormat,
  );
}

async function handleCreateToken(
  request: Request,
  auth: Auth,
  actor: Actor,
): Promise<Response> {
  const form = await request.formData();
  const name = ((form.get("name") as string) ?? "").trim();
  if (!name) return redirect("/settings");

  const created = await auth.api.createApiKey({
    body: { name },
    headers: request.headers,
  });

  // Re-render settings page with the new key shown once
  return handleSettings(request, auth, actor, created.key);
}

async function handleRevokeToken(
  request: Request,
  auth: Auth,
  keyId: string,
): Promise<Response> {
  // better-auth only deletes keys owned by the session's user.
  await auth.api
    .deleteApiKey({ body: { keyId }, headers: request.headers })
    .catch(() => undefined);
  return redirect("/settings");
}

// ---------------------------------------------------------------------------
// Settings pages
// ---------------------------------------------------------------------------

function renderSettingsAuthRequiredPage(
  pageFormat: PageFormatSelection,
): Response {
  const body = `# ripgit auth settings

Authentication required.

Settings path: \`/settings\`

${renderTextActions([
    {
      method: "GET",
      path: "/login?next=/settings",
      description: "start GitHub sign-in in a browser",
    },
    {
      method: "GET",
      path: "/",
      description: "open the auth worker landing page",
    },
  ])}${renderTextHints([
    textNavigationHint(pageFormat),
    "Browser login creates a session cookie; long-lived agent tokens are created after signing in at `/settings`.",
    "If you already have a token, send it as `Authorization: Bearer TOKEN` or as the password in basic auth to avoid the browser login redirect.",
  ])}`;
  return respondPage(body, pageFormat, 401);
}

function renderSettingsPageHtml(
  actorName: string,
  tokens: { agentId: string; name: string }[],
  origin: string,
  newToken?: string,
): string {
  const host = origin.replace(/^https?:\/\//, "");

  const newTokenBanner = newToken
    ? `<div class="banner banner-success">
        <strong>Token created — copy it now, it won't be shown again</strong>
        <div class="token-value">${escapeHtml(newToken)}</div>
        <p class="muted">Anyone with this token can push to your repos. Store it securely.</p>
        <p class="muted" style="margin-top:12px"><strong>Add as a git remote:</strong></p>
        <pre class="cmd">git remote add origin https://${escapeHtml(actorName)}:${escapeHtml(newToken)}@${escapeHtml(host)}/${escapeHtml(actorName)}/REPO-NAME
git push origin main</pre>
      </div>`
    : "";

  const tokenRows =
    tokens.length > 0
      ? `<table>
          <thead><tr><th>Name</th><th></th></tr></thead>
          <tbody>
            ${tokens
              .map(
                (t) => `<tr>
              <td>${escapeHtml(t.name)}</td>
              <td class="actions">
                <form method="POST" action="/settings/tokens/${encodeURIComponent(t.agentId)}/revoke">
                  <button class="btn-danger btn-sm" onclick="return confirm('Revoke this token?')">Revoke</button>
                </form>
              </td>
            </tr>`,
              )
              .join("")}
          </tbody>
        </table>`
      : `<p class="muted">No tokens yet.</p>`;

  return renderAuthPageHtml({
    title: "Settings",
    topbarRight: `<a href="/${encodeURIComponent(actorName)}/">Profile</a><span>·</span><strong>${escapeHtml(actorName)}</strong><span>·</span><a href="/logout">Sign out</a>`,
    mainClass: "site-shell",
    footer: authFooterHtml(),
    content: `
      ${newTokenBanner}
      <section>
        <h1>Access Tokens</h1>
        <p class="lede">Create long-lived tokens for git remotes, curl, and agents that need to browse or push through the auth worker.</p>
      </section>

      <section class="section">
        <h2>Create token</h2>
        <form method="POST" action="/settings/tokens">
          <div class="form-row">
            <input type="text" name="name" placeholder="Token name (e.g. laptop, deploy-key)" required autocomplete="off">
            <button class="btn" type="submit">Generate</button>
          </div>
        </form>
        <p class="muted" style="margin-top:8px">Generated tokens are shown exactly once in the response that creates them.</p>
      </section>

      <section class="section">
        <h2>Push a new repo</h2>
        <p class="muted">Repos are created on first push. Pick any name:</p>
        <pre class="cmd">cd my-project
git init
git add .
git commit -m "initial commit"
git remote add origin https://${escapeHtml(actorName)}:TOKEN@${escapeHtml(host)}/${escapeHtml(actorName)}/my-project
git push origin main</pre>
        <p class="muted">Replace <code>TOKEN</code> with the token you generate above. Replace <code>my-project</code> with your repo name.</p>
      </section>

      <section class="section">
        <h2>Use tokens with curl</h2>
        <pre class="cmd">curl -H "Authorization: Bearer TOKEN" ${escapeHtml(origin)}/settings?format=md</pre>
        <p class="muted">Git can also use the token as the password in a standard HTTPS remote.</p>
      </section>

      <section class="section">
        <h2>Active tokens</h2>
        ${tokenRows}
      </section>`,
  });
}

function renderSettingsPageText(
  actorName: string,
  tokens: { agentId: string; name: string }[],
  origin: string,
  newToken: string | undefined,
  pageFormat: PageFormatSelection,
): string {
  const host = origin.replace(/^https?:\/\//, "");
  const pushExample = renderIndentedBlock(
    [
      "cd my-project",
      "git init",
      "git add .",
      'git commit -m "initial commit"',
      `git remote add origin https://${actorName}:TOKEN@${host}/${actorName}/my-project`,
      "git push origin main",
    ].join("\n"),
  );

  let body = `# ripgit auth settings

Signed in as: \`${actorName}\`
Profile path: \`/${actorName}/\`
Settings path: \`/settings\`
Active tokens: \`${tokens.length}\`
`;

  if (newToken) {
    body += `
## New Token

Copy this now; it will not be shown again.

- Value: \`${newToken}\`
- Git remote: \`https://${actorName}:${newToken}@${host}/${actorName}/REPO-NAME\`
`;
  }

  body += `
## Push a New Repo

${pushExample}
`;

  body += "\n## Active Tokens\n";
  if (tokens.length === 0) {
    body += "No active tokens.\n";
  } else {
    for (const token of tokens) {
      body += `- \`${token.name}\` - revoke path: \`/settings/tokens/${encodeURIComponent(token.agentId)}/revoke\`\n`;
    }
  }

  const actions: TextAction[] = [
    {
      method: "GET",
      path: "/settings",
      description: "reload this token management page",
    },
    {
      method: "GET",
      path: `/${actorName}/`,
      description: "open your ripgit profile and repo index",
    },
    {
      method: "POST",
      path: "/settings/tokens",
      description: "create a new long-lived token",
      fields: ["`name` - label shown in the settings page"],
      requires: "authenticated session",
      effect: "returns the settings page with the new token shown once",
    },
    {
      method: "GET",
      path: "/logout?next=/",
      description: "clear the browser session and return to the landing page",
    },
  ];

  for (const token of tokens) {
    actions.push({
      method: "POST",
      path: `/settings/tokens/${encodeURIComponent(token.agentId)}/revoke`,
      description: `revoke the token named \`${token.name}\``,
      requires: "authenticated session",
      effect: "deletes the token and redirects back to `/settings`",
    });
  }

  body += renderTextActions(actions);
  body += renderTextHints([
    textNavigationHint(pageFormat),
    "Created tokens currently carry the full auth-worker scope set and should be stored like passwords.",
    "Use `Authorization: Bearer TOKEN` for API/page requests, or `https://USER:TOKEN@HOST/USER/REPO` for git remotes.",
    "Generated tokens are only shown in the response that creates them; revoking a token does not reveal its original value.",
  ]);

  return body;
}

function renderIndentedBlock(text: string): string {
  return text
    .split("\n")
    .map((line) => `    ${line}`)
    .join("\n");
}

// ---------------------------------------------------------------------------
// Landing page (shown at / when not logged in)
// ---------------------------------------------------------------------------

function renderLandingPage(
  origin: string,
  actor: Actor | null,
  pageFormat: PageFormatSelection,
): Response {
  if (pageFormat.format === "html") {
    return respondPage(renderLandingPageHtml(), pageFormat);
  }
  return respondPage(renderLandingPageText(origin, actor, pageFormat), pageFormat);
}

function renderLandingPageHtml(): string {
  return renderAuthPageHtml({
    title: "ripgit",
    topbarRight: "",
    mainClass: "landing-shell",
    footer: authFooterHtml(),
    content: `<section class="hero">
      <h1>ripgit</h1>
      <p class="tagline">A lightweight self-hosted Git server running on Cloudflare Durable Objects. Fast, searchable, yours.</p>
      <div class="cta-row">
        <a href="/login" class="signin-btn">
          <svg viewBox="0 0 16 16"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z"/></svg>
          Sign in with GitHub
        </a>
      </div>
    </section>
    <section class="feature-grid">
      <div class="feature-card">
        <h3>Git-compatible</h3>
        <p>Works with any standard git client. Push and clone with the URLs you already know.</p>
      </div>
      <div class="feature-card">
        <h3>Built-in search</h3>
        <p>Full-text search across all your code and commit history, powered by SQLite FTS5.</p>
      </div>
      <div class="feature-card">
        <h3>Edge-hosted</h3>
        <p>Runs on Cloudflare Durable Objects. No servers to manage, globally distributed.</p>
      </div>
    </section>`,
  });
}

function renderLandingPageText(
  origin: string,
  actor: Actor | null,
  pageFormat: PageFormatSelection,
): string {
  const host = origin.replace(/^https?:\/\//, "");

  if (actor) {
    let body = `# ripgit auth worker

Signed in as: \`${actor.login}\`
This auth worker fronts the ripgit backend, manages your browser session, and can mint long-lived tokens from \`/settings\`.

## Related Paths (GET paths)
- \`/${actor.login}/\`
- \`/settings\`
- \`/logout?next=/\`
`;

    body += renderTextActions([
      {
        method: "GET",
        path: `/${actor.login}/`,
        description: "open your ripgit profile and repositories",
      },
      {
        method: "GET",
        path: "/settings",
        description: "manage API keys",
      },
      {
        method: "GET",
        path: "/logout?next=/",
        description: "clear the browser session and return here",
      },
    ]);
    body += renderTextHints([
      textNavigationHint(pageFormat),
      `HTML requests to \`/\` redirect signed-in users to \`/${actor.login}/\`; text mode stays here so agents can discover the next steps.`,
      `Tokens created at \`/settings\` work with git remotes like \`https://${actor.login}:TOKEN@${host}/${actor.login}/REPO\`.`,
    ]);
    return body;
  }

  let body = `# ripgit auth worker

This worker handles GitHub sign-in, browser sessions, and long-lived tokens before forwarding requests to the ripgit backend.

Access model:
- anonymous - read-only browsing, cloning, and search
- authenticated - read plus issues and pull requests across repos
- repo owner - push, merge, and admin actions on repos under your username

## Related Paths (GET paths)
- \`/\`
- \`/login\`
- \`/settings\`
`;

  body += renderTextActions([
    {
      method: "GET",
      path: "/login",
      description: "start GitHub sign-in in a browser",
    },
    {
      method: "GET",
      path: "/settings",
      description: "open token management after sign-in",
      requires: "authenticated session",
    },
  ]);
  body += renderTextHints([
    textNavigationHint(pageFormat),
    "After signing in, the HTML landing page redirects to your profile while the text-mode landing page stays here and explains the available paths.",
    "Long-lived tokens are created from `/settings` and can then be sent as `Authorization: Bearer TOKEN` or used as the password in a standard HTTPS git remote.",
  ]);
  return body;
}


// ---------------------------------------------------------------------------
// Sign-in and sign-out
// ---------------------------------------------------------------------------

/** Only same-origin paths, so ?next= cannot bounce a user off-site. */
function safeNext(request: Request): string {
  const next = new URL(request.url).searchParams.get("next") ?? "/";
  return next.startsWith("/") && !next.startsWith("//") ? next : "/";
}

/** Redirect, carrying over any cookies better-auth set on `from`. */
function redirectWithCookies(location: string, from: Response): Response {
  const response = redirect(location);
  for (const cookie of from.headers.getSetCookie()) {
    response.headers.append("Set-Cookie", cookie);
  }
  return response;
}

async function handleLogin(request: Request, auth: Auth): Promise<Response> {
  const result = await auth.api.signInSocial({
    body: { provider: "github", callbackURL: safeNext(request) },
    headers: request.headers,
    asResponse: true,
  });
  const { url } = (await result.clone().json()) as { url?: string };
  if (!url) return new Response("GitHub sign-in is unavailable", { status: 502 });
  return redirectWithCookies(url, result);
}

async function handleLogout(request: Request, auth: Auth): Promise<Response> {
  const result = await auth.api.signOut({
    headers: request.headers,
    asResponse: true,
  });
  return redirectWithCookies(safeNext(request), result);
}

// ---------------------------------------------------------------------------
// Identity resolution — returns null for anonymous, never blocks
// ---------------------------------------------------------------------------

async function loadLogin(env: Env, userId: string): Promise<string | null> {
  const row = await env.AUTH_DB.prepare('SELECT login FROM "user" WHERE id = ?')
    .bind(userId)
    .first<{ login: string | null }>();
  return row?.login ?? null;
}

async function resolveActor(
  request: Request,
  env: Env,
  auth: Auth,
): Promise<Actor | null> {
  const token = extractToken(request);
  if (token) {
    const mirror = await env.OAUTH_KV.get(`mirror-token:${token}`);
    if (mirror) return JSON.parse(mirror) as Actor;

    const { valid, key } = await auth.api.verifyApiKey({ body: { key: token } });
    if (!valid || !key) return null;
    const login = await loadLogin(env, key.referenceId);
    if (login === null) return null;
    return {
      userId: key.referenceId,
      login,
      kind: "agent",
      keyName: key.name ?? undefined,
      scopes: USER_SCOPES,
    };
  }

  const session = await auth.api.getSession({ headers: request.headers });
  if (!session) return null;
  return {
    userId: session.user.id,
    login: session.user.login ?? "",
    kind: "user",
    scopes: USER_SCOPES,
  };
}

// ---------------------------------------------------------------------------
// GitHub Actions OIDC exchange
// ---------------------------------------------------------------------------

/** Lifetime of a minted mirror token — long enough to push, short enough to
 *  be worthless by the time it could leak out of a run log. */
const MIRROR_TOKEN_TTL = 600;

/**
 * POST /oidc/github/exchange
 *
 * Body: {"subject_token": "<GitHub Actions OIDC JWT>"}
 * Returns a short-lived bearer token usable as the password of a git remote.
 *
 * The caller proves which repository it is by presenting a token GitHub signed;
 * it does not get to say so itself. Which ripgit repo that maps to is decided
 * here, from the admin-managed allowlist, never from the request.
 */
async function handleOidcExchange(
  request: Request,
  env: Env,
): Promise<Response> {
  let subjectToken: string | undefined;
  const contentType = request.headers.get("Content-Type") ?? "";
  try {
    if (contentType.includes("application/json")) {
      subjectToken = ((await request.json()) as { subject_token?: string })
        .subject_token;
    } else {
      subjectToken =
        (await request.formData()).get("subject_token")?.toString() ??
        undefined;
    }
  } catch {
    return oidcError("invalid_request", "could not parse request body", 400);
  }

  if (!subjectToken) {
    return oidcError("invalid_request", "subject_token is required", 400);
  }
  if (!env.OIDC_AUDIENCE) {
    // Refuse rather than fall back to GitHub's default audience, which is
    // shared across every service the owner runs.
    return oidcError(
      "server_error",
      "OIDC_AUDIENCE is not configured on this deployment",
      500,
    );
  }

  try {
    const claims = await verifyGitHubOidcToken(
      subjectToken,
      env.OIDC_AUDIENCE,
      env.OAUTH_KV,
    );

    const grant = await lookupMirrorGrant(env.OAUTH_KV, claims.repository);
    if (!grant) {
      return oidcError(
        "access_denied",
        `${claims.repository} is not enrolled for mirroring`,
        403,
      );
    }
    if (grant.refs && (!claims.ref || !grant.refs.includes(claims.ref))) {
      return oidcError(
        "access_denied",
        `ref ${claims.ref ?? "(none)"} may not mirror ${claims.repository}`,
        403,
      );
    }

    const [targetOwner] = grant.target.split("/");
    const token = generateToken();
    const actor: Actor = {
      // Not a person: ripgit authorizes a mirror agent by its repo scope alone,
      // on exactly the repository the allowlist chose.
      userId: "",
      login: targetOwner,
      kind: "mirror",
      keyName: `github-actions:${claims.repository}`,
      // `mirror` distinguishes this from a human token: a mirrored repo
      // refuses direct pushes, but the mirror agent's own push is the
      // mechanism that delivers commits.
      scopes: ["push", "mirror"],
      repoScope: grant.target,
      // Anything not explicitly public is treated as private, so "internal"
      // and an absent claim both fail closed rather than publishing a mirror.
      repoVisibility: claims.repository_visibility === "public" ? "public" : "private",
    };
    await env.OAUTH_KV.put(`mirror-token:${token}`, JSON.stringify(actor), {
      expirationTtl: MIRROR_TOKEN_TTL,
    });

    return Response.json({
      access_token: token,
      token_type: "Bearer",
      expires_in: MIRROR_TOKEN_TTL,
      target: grant.target,
    });
  } catch (err) {
    if (err instanceof OidcError) {
      return oidcError("invalid_grant", err.message, 401);
    }
    throw err;
  }
}

function oidcError(
  error: string,
  description: string,
  status: number,
): Response {
  return Response.json({ error, error_description: description }, { status });
}

/**
 * Enforce a mirror token's repo scope.
 *
 * ripgit's ownership check is owner-wide, so without this a workflow in one
 * repo could push to any repo under the same owner. Returns null when allowed.
 */
function denyOutOfScope(
  request: Request,
  actor: Actor | null,
): Response | null {
  if (!actor?.repoScope) return null;
  const parts = new URL(request.url).pathname
    .replace(/^\/+/, "")
    .split("/");
  if (parts.length < 2 || !parts[0] || !parts[1]) {
    return new Response("Forbidden: token is scoped to a single repository", {
      status: 403,
    });
  }
  const requested = `${parts[0]}/${parts[1]}`.toLowerCase();
  if (requested !== actor.repoScope.toLowerCase()) {
    return new Response(
      `Forbidden: token is scoped to ${actor.repoScope}`,
      { status: 403 },
    );
  }
  return null;
}


// ---------------------------------------------------------------------------
// Forward to ripgit
// ---------------------------------------------------------------------------

function forwardToRipgit(
  request: Request,
  actor: Actor | null,
  env: Env,
): Promise<Response> {
  const outOfScope = denyOutOfScope(request, actor);
  if (outOfScope) return Promise.resolve(outOfScope);

  const headers = new Headers(request.headers);

  // Strip any ripgit headers the caller supplied before setting our own. ripgit
  // treats these as proof of identity, so a forged X-Ripgit-Actor-Id would be
  // full write access to someone else's repos, and X-Ripgit-Actor-Scopes:mirror
  // would walk past the mirror divergence guard. Nothing from the public
  // internet may reach ripgit under these names.
  for (const name of [...headers.keys()]) {
    if (name.toLowerCase().startsWith("x-ripgit-")) {
      headers.delete(name);
    }
  }

  if (actor) {
    headers.set("X-Ripgit-Actor-Id", actor.userId);
    headers.set("X-Ripgit-Actor-Name", actor.login);
    headers.set("X-Ripgit-Actor-Kind", actor.kind);
    headers.set("X-Ripgit-Actor-Scopes", actor.scopes.join(","));
    if (actor.keyName) {
      headers.set("X-Ripgit-Actor-Display-Name", actor.keyName);
    }
    if (actor.repoScope) {
      headers.set("X-Ripgit-Actor-Repo", actor.repoScope);
    }
    if (actor.repoVisibility) {
      headers.set("X-Ripgit-Repo-Visibility", actor.repoVisibility);
    }
  }
  headers.delete("Authorization");
  headers.delete("Cookie");
  return env.RIPGIT.fetch(new Request(request, { headers }));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function extractToken(request: Request): string | null {
  const auth = request.headers.get("Authorization") ?? "";
  if (auth.startsWith("Bearer ")) {
    const t = auth.slice(7).trim();
    return t || null;
  }
  if (auth.startsWith("Basic ")) {
    try {
      const decoded = atob(auth.slice(6));
      const colon = decoded.indexOf(":");
      if (colon >= 0) {
        const t = decoded.slice(colon + 1);
        return t || null;
      }
    } catch {
      /* malformed base64 */
    }
  }
  return null;
}

/**
 * Redirect to a relative or absolute URL.
 * Response.redirect() only accepts absolute URLs in Cloudflare Workers,
 * so use this helper for any relative paths.
 */
function redirect(location: string, status = 302): Response {
  return new Response(null, { status, headers: { Location: location } });
}

function generateToken(): string {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}
