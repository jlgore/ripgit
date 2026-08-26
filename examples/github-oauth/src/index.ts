/**
 * ripgit auth worker — GitHub OAuth example
 *
 * Access model:
 *   Anonymous              → read-only (browse, clone, search)
 *   Authenticated          → read + open issues/PRs on any repo
 *   Authenticated + owner  → write (push, merge, admin) on your own repos
 *
 * Routes handled here (everything else forwarded to ripgit):
 *   GET  /               → auth landing page (HTML or text mode)
 *   GET  /login          → start GitHub OAuth login, ?next= redirect after
 *   GET  /logout         → clear session cookie
 *   GET  /settings       → token management page (HTML or text mode, requires login)
 *   POST /settings/tokens                → create access token
 *   POST /settings/tokens/:id/revoke     → revoke access token
 *   GET  /oauth/authorize → OAuth provider flow (programmatic clients)
 *   GET  /oauth/callback  → unified GitHub callback
 *   POST /oauth/token     → token exchange (OAuthProvider internal)
 *   *    /admin/*         → agent management API (requires admin scope)
 *
 * Setup:
 *   1. Create a GitHub OAuth App (https://github.com/settings/applications/new)
 *      Callback URL: https://your-worker.workers.dev/oauth/callback
 *      (local dev:   http://localhost:8787/oauth/callback)
 *   2. wrangler kv namespace create OAUTH_KV  → fill IDs in wrangler.toml
 *   3. wrangler secret put GITHUB_CLIENT_SECRET
 *   4. wrangler secret put SESSION_SECRET  (any random 32+ char string)
 *   5. Set GITHUB_CLIENT_ID in wrangler.toml [vars]
 */

import {
  OAuthProvider,
  type AuthRequest,
  type ResolveExternalTokenInput,
  type ResolveExternalTokenResult,
} from "@cloudflare/workers-oauth-provider";
import { WorkerEntrypoint } from "cloudflare:workers";
import { exchangeCode, fetchGitHubUser, githubAuthorizeUrl } from "./github";
import type { ActorProps, Env } from "./types";
import {
  lookupMirrorGrant,
  OidcError,
  verifyGitHubOidcToken,
} from "./oidc";

const ALL_SCOPES = [
  "repo:read",
  "repo:write",
  "issue:write",
  "pr:merge",
  "admin",
] as const;

const SESSION_COOKIE = "ripgit_session";
const SESSION_MAX_AGE = 7 * 24 * 3600; // 7 days

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

// ---------------------------------------------------------------------------
// AdminHandler — WorkerEntrypoint for /admin/* routes (programmatic API).
// ctx.props populated by OAuthProvider after token validation.
// ---------------------------------------------------------------------------

export class AdminHandler extends WorkerEntrypoint<Env> {
  async fetch(request: Request): Promise<Response> {
    const actor = this.ctx.props as ActorProps;
    const url = new URL(request.url);

    if (url.pathname === "/admin/agents") {
      if (request.method === "POST") return this.createAgent(request, actor);
      if (request.method === "GET") return this.listAgents(actor);
      return new Response("Method Not Allowed", { status: 405 });
    }

    return new Response("Not Found", { status: 404 });
  }

  private async createAgent(
    request: Request,
    caller: ActorProps,
  ): Promise<Response> {
    if (!caller.scopes.includes("admin")) {
      return Response.json({ error: "admin scope required" }, { status: 403 });
    }
    let body: { name?: unknown; scopes?: unknown };
    try {
      body = (await request.json()) as { name?: unknown; scopes?: unknown };
    } catch {
      return Response.json({ error: "invalid JSON body" }, { status: 400 });
    }
    const name =
      typeof body.name === "string" && body.name.trim()
        ? body.name.trim()
        : null;
    if (!name) {
      return Response.json({ error: "name is required" }, { status: 400 });
    }
    const requested = Array.isArray(body.scopes)
      ? (body.scopes as unknown[]).filter(
          (s): s is string => typeof s === "string",
        )
      : [...ALL_SCOPES];
    const grantedScopes = requested.filter(
      (s) =>
        ALL_SCOPES.includes(s as (typeof ALL_SCOPES)[number]) &&
        caller.scopes.includes(s),
    );
    const { token, actor } = await createAgentToken(
      this.env,
      name,
      caller,
      grantedScopes,
    );
    return Response.json(
      { token, actorId: actor.actorId, name, scopes: grantedScopes },
      { status: 201 },
    );
  }

  private async listAgents(caller: ActorProps): Promise<Response> {
    if (!caller.scopes.includes("admin")) {
      return Response.json({ error: "admin scope required" }, { status: 403 });
    }
    const agents = await listAgentTokens(this.env, caller.actorId);
    return Response.json({ agents });
  }
}

// ---------------------------------------------------------------------------
// OAuthProvider — default export.
// ---------------------------------------------------------------------------

export default new OAuthProvider<Env>({
  apiRoute: "/admin/",
  apiHandler: AdminHandler,
  defaultHandler: { fetch: mainHandler },
  authorizeEndpoint: "/oauth/authorize",
  tokenEndpoint: "/oauth/token",
  clientRegistrationEndpoint: "/oauth/register",
  scopesSupported: [...ALL_SCOPES],
  accessTokenTTL: 3600,
  refreshTokenTTL: 30 * 86400,
  resolveExternalToken: async ({
    token,
    env,
  }: ResolveExternalTokenInput): Promise<ResolveExternalTokenResult | null> => {
    const raw = await (env as Env).OAUTH_KV.get(`agent:${token}`);
    if (!raw) return null;
    return { props: JSON.parse(raw) as ActorProps };
  },
});

// ---------------------------------------------------------------------------
// mainHandler — resolves actor first, routes, then forwards to ripgit
// ---------------------------------------------------------------------------

async function mainHandler(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const pageFormat = preferredPageFormat(request);

  // Resolve identity up front — available to all routes below
  const actor = await resolveActor(request, env);

  // ── Auth + settings routes ────────────────────────────────────────────────

  if (url.pathname === "/login") return handleLogin(request, env);
  if (url.pathname === "/logout") return handleLogout(request);
  if (url.pathname === "/oauth/authorize")
    return handleAuthorize(request, env);
  if (url.pathname === "/oauth/callback") return handleCallback(request, env);
  if (url.pathname === "/oidc/github/exchange" && request.method === "POST") {
    return handleOidcExchange(request, env);
  }

  if (url.pathname === "/settings") {
    if (!actor) {
      if (pageFormat.format === "html") return redirect(`/login?next=/settings`);
      return renderSettingsAuthRequiredPage(pageFormat);
    }
    return handleSettings(request, env, actor, undefined, pageFormat);
  }
  if (url.pathname === "/settings/tokens" && request.method === "POST") {
    if (!actor) return redirect(`/login?next=/settings`);
    return handleCreateToken(request, env, actor);
  }
  // /settings/tokens/:agentId/revoke
  const revokeMatch = url.pathname.match(
    /^\/settings\/tokens\/([^/]+)\/revoke$/,
  );
  if (revokeMatch && request.method === "POST") {
    if (!actor) return redirect(`/login?next=/settings`);
    // decodeURIComponent because url.pathname preserves %3A rather than
    // normalising it to ':', so "agent%3Auuid" would not match the KV key.
    return handleRevokeToken(env, actor, decodeURIComponent(revokeMatch[1]));
  }

  if (url.pathname === "/" && request.method === "GET") {
    // Logged-in users go straight to their profile page
    if (actor && pageFormat.format === "html") {
      return redirect(`/${actor.actorName}/`);
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
// Settings — browser UI for token management
// ---------------------------------------------------------------------------

async function handleSettings(
  request: Request,
  env: Env,
  actor: ActorProps,
  newToken?: string,
  pageFormat = preferredPageFormat(request),
): Promise<Response> {
  const tokens = await listAgentTokens(env, actor.actorId);
  const origin = new URL(request.url).origin;
  if (pageFormat.format === "html") {
    return respondPage(
      renderSettingsPageHtml(actor.actorName, tokens, origin, newToken),
      pageFormat,
    );
  }
  return respondPage(
    renderSettingsPageText(actor.actorName, tokens, origin, newToken, pageFormat),
    pageFormat,
  );
}

async function handleCreateToken(
  request: Request,
  env: Env,
  actor: ActorProps,
): Promise<Response> {
  const form = await request.formData();
  const name = ((form.get("name") as string) ?? "").trim();
  if (!name) return redirect("/settings");

  const { token } = await createAgentToken(env, name, actor, [...ALL_SCOPES]);

  // Re-render settings page with the new token shown once
  return handleSettings(request, env, actor, token);
}

async function handleRevokeToken(
  env: Env,
  actor: ActorProps,
  agentId: string,
): Promise<Response> {
  const indexKey = `agent-index:${actor.actorId}:${agentId}`;
  const token = await env.OAUTH_KV.get(indexKey);
  if (token) {
    await Promise.all([
      env.OAUTH_KV.delete(`agent:${token}`),
      env.OAUTH_KV.delete(indexKey),
    ]);
  }
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
      description: "start GitHub OAuth sign-in in a browser",
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
  actor: ActorProps | null,
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
  actor: ActorProps | null,
  pageFormat: PageFormatSelection,
): string {
  const host = origin.replace(/^https?:\/\//, "");

  if (actor) {
    let body = `# ripgit auth worker

Signed in as: \`${actor.actorName}\`
This auth worker fronts the ripgit backend, manages your browser session, and can mint long-lived tokens from \`/settings\`.

## Related Paths (GET paths)
- \`/${actor.actorName}/\`
- \`/settings\`
- \`/logout?next=/\`
`;

    body += renderTextActions([
      {
        method: "GET",
        path: `/${actor.actorName}/`,
        description: "open your ripgit profile and repositories",
      },
      {
        method: "GET",
        path: "/settings",
        description: "manage long-lived access tokens",
      },
      {
        method: "GET",
        path: "/logout?next=/",
        description: "clear the browser session and return here",
      },
    ]);
    body += renderTextHints([
      textNavigationHint(pageFormat),
      `HTML requests to \`/\` redirect signed-in users to \`/${actor.actorName}/\`; text mode stays here so agents can discover the next steps.`,
      `Tokens created at \`/settings\` work with git remotes like \`https://${actor.actorName}:TOKEN@${host}/${actor.actorName}/REPO\`.`,
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
- \`/oauth/authorize\`
`;

  body += renderTextActions([
    {
      method: "GET",
      path: "/login",
      description: "start GitHub OAuth sign-in in a browser",
    },
    {
      method: "GET",
      path: "/settings",
      description: "open token management after sign-in",
      requires: "authenticated session",
    },
    {
      method: "GET",
      path: "/oauth/authorize",
      description: "start the OAuth provider flow for programmatic clients",
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
// GitHub OAuth flows
// ---------------------------------------------------------------------------

async function handleLogin(request: Request, env: Env): Promise<Response> {
  const next = new URL(request.url).searchParams.get("next") ?? "/";
  const state = crypto.randomUUID();
  await env.OAUTH_KV.put(
    `state:${state}`,
    JSON.stringify({ type: "login", next }),
    { expirationTtl: 600 },
  );
  const callbackUrl = new URL("/oauth/callback", request.url).toString();
  return Response.redirect(
    githubAuthorizeUrl(env.GITHUB_CLIENT_ID, state, callbackUrl),
    302,
  );
}

async function handleAuthorize(request: Request, env: Env): Promise<Response> {
  const oauthReq = await env.OAUTH_PROVIDER.parseAuthRequest(request);
  const state = crypto.randomUUID();
  await env.OAUTH_KV.put(
    `state:${state}`,
    JSON.stringify({ type: "oauth", oauthReq }),
    { expirationTtl: 600 },
  );
  const callbackUrl = new URL("/oauth/callback", request.url).toString();
  return Response.redirect(
    githubAuthorizeUrl(env.GITHUB_CLIENT_ID, state, callbackUrl),
    302,
  );
}

async function handleCallback(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const code = url.searchParams.get("code");
  const state = url.searchParams.get("state");

  if (!code || !state) {
    return new Response("Missing code or state", { status: 400 });
  }

  const stateRaw = await env.OAUTH_KV.get(`state:${state}`);
  if (!stateRaw) {
    return new Response("Invalid or expired state — please try again", {
      status: 400,
    });
  }
  await env.OAUTH_KV.delete(`state:${state}`);

  type StateData =
    | { type: "login"; next: string }
    | { type: "oauth"; oauthReq: AuthRequest };

  const stateData = JSON.parse(stateRaw) as StateData;
  const callbackUrl = new URL("/oauth/callback", request.url).toString();

  let githubToken: string;
  try {
    githubToken = await exchangeCode(code, env, callbackUrl);
  } catch (err) {
    return new Response(`GitHub auth failed: ${(err as Error).message}`, {
      status: 400,
    });
  }

  const user = await fetchGitHubUser(githubToken);

  if (stateData.type === "login") {
    const actor: ActorProps = {
      actorId: `github:${user.id}`,
      actorName: user.login,
      actorKind: "user",
      scopes: [...ALL_SCOPES],
    };
    const sessionValue = await createSession(actor, env.SESSION_SECRET);
    return new Response(null, {
      status: 302,
      headers: {
        Location: stateData.next,
        "Set-Cookie": `${SESSION_COOKIE}=${encodeURIComponent(sessionValue)}; HttpOnly; SameSite=Lax; Path=/; Max-Age=${SESSION_MAX_AGE}`,
      },
    });
  } else {
    const { redirectTo } = await env.OAUTH_PROVIDER.completeAuthorization({
      request: stateData.oauthReq,
      userId: `github:${user.id}`,
      metadata: { githubLogin: user.login },
      scope: stateData.oauthReq.scope,
      props: {
        actorId: `github:${user.id}`,
        actorName: user.login,
        actorKind: "user",
        scopes: stateData.oauthReq.scope,
      } satisfies ActorProps,
    });
    return Response.redirect(redirectTo, 302);
  }
}

function handleLogout(request: Request): Response {
  const next = new URL(request.url).searchParams.get("next") ?? "/";
  return new Response(null, {
    status: 302,
    headers: {
      Location: next,
      "Set-Cookie": `${SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0`,
    },
  });
}

// ---------------------------------------------------------------------------
// KV helpers for agent tokens
// ---------------------------------------------------------------------------

async function createAgentToken(
  env: Env,
  name: string,
  caller: ActorProps,
  scopes: string[],
): Promise<{ token: string; actor: ActorProps }> {
  const token = generateToken();
  const agentId = `agent:${crypto.randomUUID()}`;
  const actor: ActorProps = {
    actorId: agentId,
    actorName: name,
    actorKind: "agent",
    ownerActorId: caller.actorId,
    ownerActorName: caller.actorName, // caller's GitHub username — used for repo ownership checks
    scopes,
  };
  await env.OAUTH_KV.put(`agent:${token}`, JSON.stringify(actor));
  await env.OAUTH_KV.put(`agent-index:${caller.actorId}:${agentId}`, token);
  return { token, actor };
}

async function listAgentTokens(
  env: Env,
  actorId: string,
): Promise<{ agentId: string; name: string }[]> {
  const prefix = `agent-index:${actorId}:`;
  const list = await env.OAUTH_KV.list({ prefix });
  const results = await Promise.all(
    list.keys.map(async (k) => {
      const agentId = k.name.slice(prefix.length);
      const token = await env.OAUTH_KV.get(k.name);
      if (!token) return null;
      const raw = await env.OAUTH_KV.get(`agent:${token}`);
      if (!raw) return null;
      const a = JSON.parse(raw) as ActorProps;
      return { agentId, name: a.actorName };
    }),
  );
  return results.filter((r): r is { agentId: string; name: string } => r !== null);
}

// ---------------------------------------------------------------------------
// Session cookie helpers — HMAC-SHA256 signed, stateless
// ---------------------------------------------------------------------------

async function createSession(
  actor: ActorProps,
  secret: string,
): Promise<string> {
  const payload = btoa(JSON.stringify(actor));
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign(
    "HMAC",
    key,
    new TextEncoder().encode(payload),
  );
  const sigHex = Array.from(new Uint8Array(sig))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
  return `${payload}.${sigHex}`;
}

async function verifySession(
  value: string,
  secret: string,
): Promise<ActorProps | null> {
  const dot = value.lastIndexOf(".");
  if (dot < 0) return null;
  const payload = value.slice(0, dot);
  const sigHex = value.slice(dot + 1);
  let sigBytes: Uint8Array;
  try {
    const pairs = sigHex.match(/.{2}/g) ?? [];
    sigBytes = Uint8Array.from(pairs.map((h) => parseInt(h, 16)));
  } catch {
    return null;
  }
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["verify"],
  );
  const valid = await crypto.subtle.verify(
    "HMAC",
    key,
    sigBytes,
    new TextEncoder().encode(payload),
  );
  if (!valid) return null;
  try {
    return JSON.parse(atob(payload)) as ActorProps;
  } catch {
    return null;
  }
}

function getSessionCookie(request: Request): string | null {
  const cookies = request.headers.get("Cookie") ?? "";
  const match = cookies.match(/(?:^|;\s*)ripgit_session=([^;]+)/);
  return match ? decodeURIComponent(match[1]) : null;
}

// ---------------------------------------------------------------------------
// Identity resolution — returns null for anonymous, never blocks
// ---------------------------------------------------------------------------

async function resolveActor(
  request: Request,
  env: Env,
): Promise<ActorProps | null> {
  const token = extractToken(request);
  if (token) {
    const agentRaw = await env.OAUTH_KV.get(`agent:${token}`);
    if (agentRaw) return JSON.parse(agentRaw) as ActorProps;
    const tokenData = await env.OAUTH_PROVIDER.unwrapToken<ActorProps>(token);
    if (tokenData && tokenData.expiresAt > Math.floor(Date.now() / 1000)) {
      return tokenData.grant.props;
    }
  }
  const cookieValue = getSessionCookie(request);
  if (cookieValue) {
    return verifySession(cookieValue, env.SESSION_SECRET);
  }
  return null;
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
    const actor: ActorProps = {
      actorId: `agent:gha:${claims.repository}`,
      actorName: `github-actions:${claims.repository}`,
      actorKind: "agent",
      // Ownership in ripgit is checked against the *target* owner, which is the
      // allowlist's decision — not against the GitHub owner in the token.
      ownerActorName: targetOwner,
      // `mirror` distinguishes this from a human token: a mirrored repo
      // refuses direct pushes, but the mirror agent's own push is the
      // mechanism that delivers commits.
      scopes: ["push", "mirror"],
      repoScope: grant.target,
    };
    await env.OAUTH_KV.put(`agent:${token}`, JSON.stringify(actor), {
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
  actor: ActorProps | null,
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
  actor: ActorProps | null,
  env: Env,
): Promise<Response> {
  const outOfScope = denyOutOfScope(request, actor);
  if (outOfScope) return Promise.resolve(outOfScope);

  const headers = new Headers(request.headers);
  if (actor) {
    // For ownership checks in ripgit, what matters is the GitHub username of the
    // person who owns the repos. For agents, that's ownerActorName, not actorName
    // (actorName is the token's display name, e.g. "laptop").
    const ownerName =
      actor.actorKind === "agent" && actor.ownerActorName
        ? actor.ownerActorName
        : actor.actorName;

    headers.set("X-Ripgit-Actor-Id", actor.actorId);
    headers.set("X-Ripgit-Actor-Name", ownerName);        // GitHub username for ownership
    headers.set("X-Ripgit-Actor-Display-Name", actor.actorName); // token name for audit/display
    headers.set("X-Ripgit-Actor-Kind", actor.actorKind);
    headers.set("X-Ripgit-Actor-Scopes", actor.scopes.join(","));
    if (actor.ownerActorId) {
      headers.set("X-Ripgit-Actor-Owner", actor.ownerActorId);
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
