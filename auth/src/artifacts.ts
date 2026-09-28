import type { Actor } from "./types";
import { escapeHtml as h, preferredPageFormat, redirect, renderAuthPageHtml, respondPage } from "./pages";

type Forward = (request: Request) => Promise<Response>;
interface ArtifactRepo { name: string; description?: string; status?: string; defaultBranch: string; }
interface Link { local: string; repo: string | null; last_sync: string | null; }
interface Listing { repos: ArtifactRepo[]; links: Link[]; cursor?: string; links_cursor?: string; total: number; }
const path = "/settings/artifacts";
const localName = /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,99}$/;

/** Browser UI; all namespace and repository permissions are checked by core. */
export async function handleArtifacts(request: Request, actor: Actor | null, forward: Forward): Promise<Response> {
  const url = new URL(request.url);
  if (!actor || actor.kind !== "user") return redirect(`/login?next=${path}`);
  const format = preferredPageFormat(request);
  const api = (route: string, body?: object) => forward(new Request(new URL(route, url.origin), {
    method: body ? "POST" : "GET",
    headers: body ? { "Content-Type": "application/json", Origin: url.origin } : {},
    body: body ? JSON.stringify(body) : undefined,
  }));
  const page = (content: string, status = 200, text?: string): Response => {
    const response = respondPage(format.format === "html" ? renderAuthPageHtml({
      title: "Artifacts", mainClass: "site-shell",
      topbarRight: `<a href="/${encodeURIComponent(actor.login)}/">Profile</a><span>·</span><a href="/settings">Settings</a><span>·</span><a href="/logout">Sign out</a>`,
      content,
    }) : (text ?? content.replace(/<[^>]*>/g, " ")), format, status);
    response.headers.set("Cache-Control", "private, no-store");
    return response;
  };
  const error = (message: string, status: number) => page(`<h1>Artifacts</h1><p role="alert">${h(message)}</p><p><a href="${path}">Back to Artifacts</a></p>`, status);

  if (request.method !== "GET" && request.method !== "POST") return error("Method not allowed", 405);
  // Check before reading forms or issuing mutations. The binding is deployment-wide.
  const query = new URLSearchParams();
  for (const key of ["cursor", "links_cursor"]) {
    const value = url.searchParams.get(key);
    if (value) query.set(key, value);
  }
  const listingResponse = await api(`/api/artifacts?${query}`);
  if (!listingResponse.ok) return error(listingResponse.status === 403
    ? "Artifacts management is restricted to deployment operators."
    : "The Artifacts namespace is unavailable. Please retry shortly.", listingResponse.status);
  const listing = await listingResponse.json<Listing>();

  if (request.method === "POST") {
    if (request.headers.get("Origin") !== url.origin) return error("Invalid request origin", 403);
    const form = await request.formData();
    const local = String(form.get("local") ?? "").trim();
    const action = String(form.get("action") ?? "");
    if (!localName.test(local)) return error("Use 1–100 letters, digits, dots, dashes or underscores for the local name; start with a letter or digit.", 400);
    const base = `/${encodeURIComponent(actor.login)}/${encodeURIComponent(local)}/artifacts`;
    let result: Response;
    if (action === "sync") {
      result = await api(`${base}/sync`, {});
    } else if (action === "link") {
      const repo = String(form.get("repo") ?? "").trim();
      if (!repo || repo.length > 255) return error("Choose an Artifacts repository.", 400);
      result = await api(`${base}/link`, { repo });
    } else if (action === "import") {
      const source = String(form.get("source") ?? "").trim();
      let remote: URL;
      try { remote = new URL(source); } catch { return error("Enter a public HTTPS Git URL.", 400); }
      if (remote.protocol !== "https:" || remote.username || remote.password || remote.search || remote.hash) return error("Use a public HTTPS Git URL without credentials, query parameters or fragments.", 400);
      const name = String(form.get("name") ?? "").trim();
      if (!localName.test(name)) return error("Choose a valid name for the imported Artifacts repository.", 400);
      result = await api(`${base}/link`, { import: remote.href, name });
    } else return error("Unknown action", 400);
    if (!result.ok) {
      const detail = result.status === 409 ? await result.text() : "The operation failed. The link may already have been created; inspect its status before retrying. Imports must finish before syncing.";
      return error(detail, result.status);
    }
    return redirect(`${path}?local=${encodeURIComponent(local)}&done=${action}`);
  }

  const selected = url.searchParams.get("local");
  let notice = "";
  if (selected && localName.test(selected)) {
    const result = await api(`/${encodeURIComponent(actor.login)}/${encodeURIComponent(selected)}/artifacts`);
    if (result.ok) {
      const status = await result.json<{ linked: boolean; repo: string | null; last_sync: string | null }>();
      if (status.linked) {
        notice = `<section class="section"><h2>${h(selected)}</h2><p>Linked to ${h(status.repo ?? "external remote")}. ${status.last_sync ? "Last sync: " + h(status.last_sync) : "Not synced yet. If importing, wait until its status is ready before syncing."}</p>${syncForm(selected)} <a href="/${encodeURIComponent(actor.login)}/${encodeURIComponent(selected)}/">Open repository</a></section>`;
      }
    }
  }
  function syncForm(local: string): string {
    return `<form method="POST" action="${path}"><input type="hidden" name="action" value="sync"><input type="hidden" name="local" value="${h(local)}"><button class="btn" type="submit">Sync now</button></form>`;
  }
  const linked = listing.links.map(link => `<li style="padding:12px 0"><a href="/${encodeURIComponent(actor.login)}/${encodeURIComponent(link.local)}/">${h(actor.login)}/${h(link.local)}</a> ← ${h(link.repo ?? "external remote")}<p class="muted">${link.last_sync ? "Last sync: " + h(link.last_sync) : "Not synced yet"}</p>${syncForm(link.local)}</li>`).join("");
  const repos = listing.repos.map(repo => {
    const busy = repo.status && repo.status !== "ready";
    const suggested = repo.name.replace(/[^a-zA-Z0-9._-]/g, "-").replace(/^[^a-zA-Z0-9]+/, "").slice(0, 100);
    return `<li style="padding:16px 0;border-bottom:1px solid #d1d9e0"><strong>${h(repo.name)}</strong> <span class="muted">${h(repo.status ?? "ready")} · ${h(repo.defaultBranch)}</span>${repo.description ? `<p>${h(repo.description)}</p>` : ""}<form method="POST" action="${path}"><input type="hidden" name="action" value="link"><input type="hidden" name="repo" value="${h(repo.name)}"><div class="form-row"><label>Local repository name <input type="text" name="local" value="${h(suggested)}" pattern="[a-zA-Z0-9][a-zA-Z0-9._-]{0,99}" required maxlength="100"></label><button class="btn" type="submit" ${busy ? "disabled" : ""}>Link privately</button></div></form></li>`;
  }).join("");
  const text = [
    "# Artifacts", "", "New links are private. Link or import, then sync to browse code.",
    "", "## Linked repositories", ...listing.links.map(link => `- ${JSON.stringify(link.local)}: /${actor.login}/${encodeURIComponent(link.local)}/`),
    "", "## Namespace repositories", ...listing.repos.map(repo => `- ${JSON.stringify(repo.name)} (${repo.status ?? "ready"})`),
    "", "## Actions (browser session required)",
    `- POST ${path}: action=link, repo=<Artifacts name>, local=<new local name>`,
    `- POST ${path}: action=import, source=<public HTTPS Git URL>, name=<new Artifacts name>, local=<new local name>`,
    `- POST ${path}: action=sync, local=<linked local name>`,
    `- GET ${path}: refresh status`,
    ...(listing.cursor ? [`- GET ${path}?cursor=${encodeURIComponent(listing.cursor)}: next namespace page`] : []),
    ...(listing.links_cursor ? [`- GET ${path}?links_cursor=${encodeURIComponent(listing.links_cursor)}: next linked page`] : []),
    "", "POST forms must send a matching Origin header.",
  ].join("\n");
  return page(`<h1>Artifacts</h1><p class="lede">Browse the deployment’s Artifacts namespace and bring repositories into ripgit.</p><p>New links are private under <strong>${h(actor.login)}</strong>. Sync copies their code into ripgit; later upstream changes need another sync. <a href="${path}">Refresh status</a></p>${notice}
    <section class="section"><h2>Linked repositories</h2>${linked ? `<ul style="list-style:none;padding:0">${linked}</ul>` : `<p class="muted">No linked repositories yet. Link one below, then sync to browse its code. New links may take a moment to appear here.</p>`}${listing.links_cursor ? `<a href="${path}?links_cursor=${encodeURIComponent(listing.links_cursor)}">More linked repositories</a>` : ""}</section>
    <section class="section"><h2>Namespace repositories (${listing.total})</h2>${repos ? `<ul style="list-style:none;padding:0">${repos}</ul>` : `<p class="muted">No repositories on this page. Import a public repository below to get started.</p>`}${listing.cursor ? `<a href="${path}?cursor=${encodeURIComponent(listing.cursor)}">Next page</a>` : ""}</section>
    <section class="section"><h2>Import a public repository</h2><p>Imports create a repository in Artifacts and a private link here. When the import is ready, click Sync now.</p><form method="POST" action="${path}"><input type="hidden" name="action" value="import"><p><label>Public HTTPS Git URL <input type="url" name="source" placeholder="https://github.com/owner/repo.git" required></label></p><p><label>New Artifacts name <input type="text" name="name" required pattern="[a-zA-Z0-9][a-zA-Z0-9._-]{0,99}" maxlength="100"></label></p><p><label>New local repository name <input type="text" name="local" required pattern="[a-zA-Z0-9][a-zA-Z0-9._-]{0,99}" maxlength="100"></label></p><button class="btn" type="submit">Import and link privately</button></form></section>`, 200, text);
}
