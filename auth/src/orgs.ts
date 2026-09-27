/**
 * Organization pages: create orgs, manage members and teams, and set the
 * default role members get on the org's repos.
 *
 *   GET  /orgs                                        your orgs (+ create form)
 *   POST /orgs                                        create: name, slug
 *   GET  /orgs/:slug                                  members, teams, settings
 *   POST /orgs/:slug/members                          add: login, role
 *   POST /orgs/:slug/members/:memberId/role           change role: role
 *   POST /orgs/:slug/members/:memberId/remove
 *   POST /orgs/:slug/teams                            create: name
 *   POST /orgs/:slug/teams/:teamId/remove
 *   POST /orgs/:slug/teams/:teamId/members            add: login
 *   POST /orgs/:slug/teams/:teamId/members/:userId/remove
 *   POST /orgs/:slug/settings                         default_repo_role
 *
 * Pages live under /orgs/ rather than /:org/ because /:owner/:repo belongs to
 * ripgit; "orgs" is a reserved namespace so no org can shadow these routes.
 *
 * Reads come straight from D1. Writes go through better-auth, which checks the
 * caller's org permissions itself -- except addMember, a server-only endpoint
 * that trusts its caller, so that path checks for owner/admin first. People
 * are added by GitHub login, which works once they have signed in; this
 * instance is small enough that email invitations are not worth the plumbing.
 */

import { APIError } from "better-auth/api";
import type { Auth } from "./auth";
import {
  escapeHtml,
  redirect,
  renderAuthPageHtml,
  renderTextActions,
  renderTextHints,
  respondPage,
  textNavigationHint,
  type PageFormatSelection,
  type TextAction,
} from "./pages";
import type { Actor, Env } from "./types";

const ORG_ROLES = ["member", "admin", "owner"] as const;
const DEFAULT_REPO_ROLES = ["none", "read", "write"] as const;

interface OrgRow {
  id: string;
  name: string;
  slug: string;
}

interface MemberRow {
  memberId: string;
  userId: string;
  login: string | null;
  name: string;
  role: string;
}

interface TeamRow {
  id: string;
  name: string;
}

interface TeamMemberRow {
  teamId: string;
  userId: string;
  login: string | null;
}

interface OrgPage {
  org: OrgRow;
  viewerIsAdmin: boolean;
  members: MemberRow[];
  teams: (TeamRow & { members: TeamMemberRow[] })[];
  defaultRepoRole: string;
}

/** Handle /orgs routes; null when the path is not ours. */
export async function handleOrgs(
  request: Request,
  env: Env,
  auth: Auth,
  actor: Actor | null,
  pageFormat: PageFormatSelection,
): Promise<Response | null> {
  const url = new URL(request.url);
  const parts = url.pathname.replace(/^\/+|\/+$/g, "").split("/");
  if (parts[0] !== "orgs") return null;

  // Org management needs a browser session: better-auth checks permissions
  // against the session, and API keys do not carry one.
  if (!actor || actor.kind !== "user") {
    if (pageFormat.format === "html") {
      return redirect(`/login?next=${encodeURIComponent(url.pathname)}`);
    }
    return respondPage(
      "# Organizations\n\nSign in with a browser session to manage organizations.\n",
      pageFormat,
      401,
    );
  }

  const post = request.method === "POST";
  const segs = parts.slice(1).map(decodeURIComponent);

  try {
    if (segs.length === 0) {
      if (post) return await createOrg(request, auth);
      if (request.method === "GET") return await listOrgsPage(env, actor, pageFormat);
    }

    const org = segs[0] ? await orgBySlug(env, segs[0]) : null;
    const membership = org ? await memberOf(env, org.id, actor.userId) : null;
    // Non-members get a 404: org membership is not public information.
    if (!org || !membership) return notFound(pageFormat);
    const isAdmin = isAdminRole(membership.role);

    if (segs.length === 1 && request.method === "GET") {
      return await orgPage(env, org, isAdmin, pageFormat);
    }
    if (!post) return new Response("Method Not Allowed", { status: 405 });

    const form = await readForm(request);
    const field = (name: string) => String(form.get(name) ?? "").trim();
    const back = redirect(`/orgs/${encodeURIComponent(org.slug)}`, 303);
    const [, section, id, action, subId, subAction] = segs;
    const headers = request.headers;
    const organizationId = org.id;

    if (section === "members" && !id) {
      // addMember trusts its caller; this is the permission check.
      if (!isAdmin) return forbidden();
      const role = field("role");
      if (!isOrgRole(role)) return badRequest(`role must be one of ${ORG_ROLES.join(", ")}`);
      if (role === "owner" && !membership.role.split(",").includes("owner")) {
        return forbidden("only an owner can add another owner");
      }
      const userId = await userIdByLogin(env, field("login"));
      if (!userId) return badRequest(`no user with login "${field("login")}" has signed in yet`);
      await auth.api.addMember({ body: { userId, role, organizationId } });
      return back;
    }
    if (section === "members" && id && action === "role") {
      const role = field("role");
      if (!isOrgRole(role)) return badRequest(`role must be one of ${ORG_ROLES.join(", ")}`);
      await auth.api.updateMemberRole({ body: { memberId: id, role, organizationId }, headers });
      return back;
    }
    if (section === "members" && id && action === "remove") {
      await auth.api.removeMember({ body: { memberIdOrEmail: id, organizationId }, headers });
      return back;
    }
    if (section === "teams" && !id) {
      await auth.api.createTeam({ body: { name: field("name"), organizationId }, headers });
      return back;
    }
    if (section === "teams" && id && action === "remove") {
      await auth.api.removeTeam({ body: { teamId: id, organizationId }, headers });
      return back;
    }
    if (section === "teams" && id && action === "members" && !subId) {
      const userId = await userIdByLogin(env, field("login"));
      if (!userId) return badRequest(`no user with login "${field("login")}" has signed in yet`);
      await auth.api.addTeamMember({ body: { teamId: id, userId, organizationId }, headers });
      return back;
    }
    if (section === "teams" && id && action === "members" && subId && subAction === "remove") {
      await auth.api.removeTeamMember({
        body: { teamId: id, userId: subId, organizationId },
        headers,
      });
      return back;
    }
    if (section === "settings" && !id) {
      if (!isAdmin) return forbidden();
      const role = field("default_repo_role");
      if (!(DEFAULT_REPO_ROLES as readonly string[]).includes(role)) {
        return badRequest(`default_repo_role must be one of ${DEFAULT_REPO_ROLES.join(", ")}`);
      }
      await env.AUTH_DB.prepare(
        `INSERT INTO ripgit_org_settings (org_id, default_repo_role) VALUES (?, ?)
         ON CONFLICT (org_id) DO UPDATE SET default_repo_role = excluded.default_repo_role`,
      )
        .bind(organizationId, role)
        .run();
      return back;
    }
    return notFound(pageFormat);
  } catch (err) {
    if (err instanceof APIError) {
      const status = err.statusCode >= 400 && err.statusCode < 500 ? err.statusCode : 400;
      return new Response(err.message || "request refused", { status });
    }
    throw err;
  }
}

/**
 * The submitted form, or an empty one. Action-only POSTs (remove, delete) carry
 * no body, and formData() throws when there is no Content-Type to parse by.
 */
async function readForm(request: Request): Promise<FormData> {
  const type = request.headers.get("Content-Type") ?? "";
  if (
    type.includes("application/x-www-form-urlencoded") ||
    type.includes("multipart/form-data")
  ) {
    return request.formData();
  }
  return new FormData();
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

function isAdminRole(role: string): boolean {
  return role.split(",").some((r) => r.trim() === "owner" || r.trim() === "admin");
}

function isOrgRole(role: string): role is (typeof ORG_ROLES)[number] {
  return (ORG_ROLES as readonly string[]).includes(role);
}

async function orgBySlug(env: Env, slug: string): Promise<OrgRow | null> {
  return env.AUTH_DB.prepare("SELECT id, name, slug FROM organization WHERE slug = ?")
    .bind(slug.toLowerCase())
    .first<OrgRow>();
}

async function memberOf(
  env: Env,
  organizationId: string,
  userId: string,
): Promise<{ id: string; role: string } | null> {
  return env.AUTH_DB.prepare(
    `SELECT id, role FROM member WHERE "organizationId" = ? AND "userId" = ?`,
  )
    .bind(organizationId, userId)
    .first<{ id: string; role: string }>();
}

async function userIdByLogin(env: Env, login: string): Promise<string | null> {
  if (!login) return null;
  const row = await env.AUTH_DB.prepare('SELECT id FROM "user" WHERE login = ?')
    .bind(login.toLowerCase())
    .first<{ id: string }>();
  return row?.id ?? null;
}

async function loadOrgPage(env: Env, org: OrgRow, viewerIsAdmin: boolean): Promise<OrgPage> {
  const [members, teams, teamMembers, settings] = await env.AUTH_DB.batch([
    env.AUTH_DB.prepare(
      `SELECT m.id AS memberId, u.id AS userId, u.login, u.name, m.role
       FROM member m JOIN "user" u ON u.id = m."userId"
       WHERE m."organizationId" = ? ORDER BY u.login`,
    ).bind(org.id),
    env.AUTH_DB.prepare(
      `SELECT id, name FROM team WHERE "organizationId" = ? ORDER BY name`,
    ).bind(org.id),
    env.AUTH_DB.prepare(
      `SELECT tm."teamId" AS teamId, u.id AS userId, u.login
       FROM "teamMember" tm
       JOIN team t ON t.id = tm."teamId"
       JOIN "user" u ON u.id = tm."userId"
       WHERE t."organizationId" = ? ORDER BY u.login`,
    ).bind(org.id),
    env.AUTH_DB.prepare(
      "SELECT default_repo_role FROM ripgit_org_settings WHERE org_id = ?",
    ).bind(org.id),
  ]);
  const byTeam = (teamMembers.results as unknown as TeamMemberRow[]).reduce(
    (acc, row) => acc.set(row.teamId, [...(acc.get(row.teamId) ?? []), row]),
    new Map<string, TeamMemberRow[]>(),
  );
  return {
    org,
    viewerIsAdmin,
    members: members.results as unknown as MemberRow[],
    teams: (teams.results as unknown as TeamRow[]).map((t) => ({
      ...t,
      members: byTeam.get(t.id) ?? [],
    })),
    // ripgit's role resolution treats a missing row as "read"; show the same.
    defaultRepoRole:
      (settings.results[0] as { default_repo_role?: string } | undefined)?.default_repo_role ??
      "read",
  };
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async function createOrg(request: Request, auth: Auth): Promise<Response> {
  const form = await readForm(request);
  const name = String(form.get("name") ?? "").trim();
  const slug = String(form.get("slug") ?? "").trim().toLowerCase() || name.toLowerCase();
  if (!name) return badRequest("name is required");
  // The create hook in auth.ts validates the slug and claims the namespace.
  const org = await auth.api.createOrganization({
    body: { name, slug },
    headers: request.headers,
  });
  return redirect(`/orgs/${encodeURIComponent(org?.slug ?? slug)}`, 303);
}

async function listOrgsPage(
  env: Env,
  actor: Actor,
  pageFormat: PageFormatSelection,
): Promise<Response> {
  const { results } = await env.AUTH_DB.prepare(
    `SELECT o.slug, o.name, m.role FROM member m
     JOIN organization o ON o.id = m."organizationId"
     WHERE m."userId" = ? ORDER BY o.slug`,
  )
    .bind(actor.userId)
    .all<{ slug: string; name: string; role: string }>();

  if (pageFormat.format !== "html") {
    let body = `# Organizations\n\nSigned in as: \`${actor.login}\`\n\n## Yours\n`;
    body += results.length
      ? results.map((o) => `- \`/orgs/${o.slug}\` - ${o.name} (${o.role})`).join("\n") + "\n"
      : "None yet.\n";
    body += renderTextActions([
      {
        method: "POST",
        path: "/orgs",
        description: "create an organization",
        fields: ["`name`", "`slug` - permanent owner name for its repos; defaults to the name"],
        requires: "a login listed in ORG_CREATORS",
      },
    ]);
    body += renderTextHints([textNavigationHint(pageFormat)]);
    return respondPage(body, pageFormat);
  }

  const rows = results
    .map(
      (o) =>
        `<tr><td><a href="/orgs/${encodeURIComponent(o.slug)}">${escapeHtml(o.slug)}</a></td><td>${escapeHtml(o.name)}</td><td>${escapeHtml(o.role)}</td></tr>`,
    )
    .join("");
  return respondPage(
    renderAuthPageHtml({
      title: "Organizations",
      topbarRight: navHtml(actor),
      content: `
      <h1>Organizations</h1>
      <p class="lede">Organizations own repos together. Members get the org's default role on its repos; teams can be granted more per repo.</p>
      ${rows ? `<table><thead><tr><th>Slug</th><th>Name</th><th>Your role</th></tr></thead><tbody>${rows}</tbody></table>` : `<p class="muted">You are not in any organizations yet.</p>`}
      <section class="section">
        <h2>Create an organization</h2>
        <form method="POST" action="/orgs">
          <div class="form-row">
            <input type="text" name="name" placeholder="Name" required autocomplete="off">
            <input type="text" name="slug" placeholder="slug (permanent, used in repo URLs)" autocomplete="off">
            <button class="btn" type="submit">Create</button>
          </div>
        </form>
        <p class="muted" style="margin-top:8px">The slug becomes the owner name in repo URLs and cannot be changed later.</p>
      </section>`,
    }),
    pageFormat,
  );
}

async function orgPage(
  env: Env,
  org: OrgRow,
  viewerIsAdmin: boolean,
  pageFormat: PageFormatSelection,
): Promise<Response> {
  const page = await loadOrgPage(env, org, viewerIsAdmin);
  if (pageFormat.format !== "html") return respondPage(renderOrgText(page, pageFormat), pageFormat);
  return respondPage(renderOrgHtml(page), pageFormat);
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function navHtml(actor: Actor): string {
  return `<a href="/orgs">Organizations</a><span>·</span><a href="/${encodeURIComponent(actor.login)}/">${escapeHtml(actor.login)}</a><span>·</span><a href="/logout">Sign out</a>`;
}

function selectHtml(name: string, options: readonly string[], selected: string): string {
  const opts = options
    .map((o) => `<option value="${o}"${o === selected ? " selected" : ""}>${o}</option>`)
    .join("");
  return `<select name="${name}">${opts}</select>`;
}

function postButton(action: string, label: string, confirm?: string): string {
  const onclick = confirm ? ` onclick="return confirm('${escapeHtml(confirm)}')"` : "";
  return `<form method="POST" action="${action}" style="display:inline"><button class="btn-danger btn-sm"${onclick}>${label}</button></form>`;
}

function renderOrgHtml(page: OrgPage): string {
  const base = `/orgs/${encodeURIComponent(page.org.slug)}`;
  const admin = page.viewerIsAdmin;

  const memberRows = page.members
    .map((m) => {
      const who = escapeHtml(m.login ?? m.name);
      const role = admin
        ? `<form method="POST" action="${base}/members/${encodeURIComponent(m.memberId)}/role" style="display:inline">${selectHtml("role", ORG_ROLES, m.role)} <button class="btn btn-sm" type="submit">Save</button></form>`
        : escapeHtml(m.role);
      const remove = admin
        ? postButton(`${base}/members/${encodeURIComponent(m.memberId)}/remove`, "Remove", `Remove ${m.login ?? m.name}?`)
        : "";
      return `<tr><td>${who}</td><td>${role}</td><td class="actions">${remove}</td></tr>`;
    })
    .join("");

  const teams = page.teams
    .map((t) => {
      const tbase = `${base}/teams/${encodeURIComponent(t.id)}`;
      const members = t.members.length
        ? t.members
            .map(
              (tm) =>
                `<li>${escapeHtml(tm.login ?? tm.userId)} ${admin ? postButton(`${tbase}/members/${encodeURIComponent(tm.userId)}/remove`, "Remove") : ""}</li>`,
            )
            .join("")
        : `<li class="muted">No members</li>`;
      const add = admin
        ? `<form method="POST" action="${tbase}/members"><div class="form-row"><input type="text" name="login" placeholder="GitHub login (an org member)" required><button class="btn btn-sm" type="submit">Add to team</button></div></form>`
        : "";
      const del = admin ? postButton(`${tbase}/remove`, "Delete team", `Delete team ${t.name}?`) : "";
      return `<div class="banner"><h2 style="margin-top:0">${escapeHtml(t.name)} ${del}</h2><p class="muted">Team id <code>${escapeHtml(t.id)}</code>, for repo grants.</p><ul style="margin:8px 0 8px 20px">${members}</ul>${add}</div>`;
    })
    .join("");

  const adminForms = admin
    ? `
      <section class="section">
        <h2>Add a member</h2>
        <form method="POST" action="${base}/members">
          <div class="form-row">
            <input type="text" name="login" placeholder="GitHub login" required autocomplete="off">
            ${selectHtml("role", ORG_ROLES, "member")}
            <button class="btn" type="submit">Add</button>
          </div>
        </form>
        <p class="muted" style="margin-top:8px">They need to have signed in to ripgit once.</p>
      </section>
      <section class="section">
        <h2>Create a team</h2>
        <form method="POST" action="${base}/teams">
          <div class="form-row">
            <input type="text" name="name" placeholder="Team name" required autocomplete="off">
            <button class="btn" type="submit">Create</button>
          </div>
        </form>
      </section>
      <section class="section">
        <h2>Default repo role</h2>
        <p class="muted">What every member gets on this organization's repos without a grant.</p>
        <form method="POST" action="${base}/settings">
          <div class="form-row">
            ${selectHtml("default_repo_role", DEFAULT_REPO_ROLES, page.defaultRepoRole)}
            <button class="btn" type="submit">Save</button>
          </div>
        </form>
      </section>`
    : "";

  return renderAuthPageHtml({
    title: page.org.name,
    topbarRight: `<a href="/orgs">Organizations</a><span>·</span><a href="/logout">Sign out</a>`,
    content: `
      <h1>${escapeHtml(page.org.name)}</h1>
      <p class="lede"><a href="/${encodeURIComponent(page.org.slug)}/">/${escapeHtml(page.org.slug)}/</a> · members default to <strong>${escapeHtml(page.defaultRepoRole)}</strong> on its repos</p>
      <section class="section">
        <h2>Members</h2>
        <table><thead><tr><th>Login</th><th>Role</th><th></th></tr></thead><tbody>${memberRows}</tbody></table>
      </section>
      <section class="section">
        <h2>Teams</h2>
        ${teams || `<p class="muted">No teams yet.</p>`}
      </section>
      ${adminForms}`,
  });
}

function renderOrgText(page: OrgPage, pageFormat: PageFormatSelection): string {
  const base = `/orgs/${page.org.slug}`;
  let body = `# ${page.org.name}\n\nOwner path: \`/${page.org.slug}/\`\nDefault repo role for members: \`${page.defaultRepoRole}\`\nYou can administer: \`${page.viewerIsAdmin}\`\n\n## Members\n`;
  for (const m of page.members) {
    body += `- \`${m.login ?? m.name}\` - ${m.role} (member id \`${m.memberId}\`)\n`;
  }
  body += "\n## Teams\n";
  if (page.teams.length === 0) body += "None.\n";
  for (const t of page.teams) {
    const who = t.members.map((tm) => `\`${tm.login ?? tm.userId}\``).join(", ") || "no members";
    body += `- \`${t.name}\` (team id \`${t.id}\`): ${who}\n`;
  }

  const actions: TextAction[] = page.viewerIsAdmin
    ? [
        { method: "POST", path: `${base}/members`, description: "add a member", fields: ["`login`", "`role` - member, admin, or owner"], requires: "org owner or admin" },
        { method: "POST", path: `${base}/members/MEMBER_ID/role`, description: "change a member's role", fields: ["`role`"], requires: "org owner or admin" },
        { method: "POST", path: `${base}/members/MEMBER_ID/remove`, description: "remove a member", requires: "org owner or admin" },
        { method: "POST", path: `${base}/teams`, description: "create a team", fields: ["`name`"], requires: "org owner or admin" },
        { method: "POST", path: `${base}/teams/TEAM_ID/members`, description: "add an org member to a team", fields: ["`login`"], requires: "org owner or admin" },
        { method: "POST", path: `${base}/teams/TEAM_ID/members/USER_ID/remove`, description: "remove someone from a team", requires: "org owner or admin" },
        { method: "POST", path: `${base}/teams/TEAM_ID/remove`, description: "delete a team", requires: "org owner or admin" },
        { method: "POST", path: `${base}/settings`, description: "set the default repo role for members", fields: ["`default_repo_role` - none, read, or write"], requires: "org owner or admin" },
      ]
    : [];
  body += renderTextActions(actions);
  body += renderTextHints([
    textNavigationHint(pageFormat),
    "Grant a team more than the default on one repo from that repo's settings page, using the team id above.",
  ]);
  return body;
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

function notFound(pageFormat: PageFormatSelection): Response {
  return respondPage("# Not found\n", pageFormat, 404);
}

function forbidden(message = "only an organization owner or admin can do that"): Response {
  return new Response(message, { status: 403 });
}

function badRequest(message: string): Response {
  return new Response(message, { status: 400 });
}
