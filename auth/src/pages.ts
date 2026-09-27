/**
 * Shared page plumbing for the auth worker: format negotiation (HTML,
 * markdown, plain text), the HTML shell, and text-mode action lists.
 */

export type PageFormat = "html" | "markdown" | "text";

export interface PageFormatSelection {
  format: PageFormat;
  varyAccept: boolean;
}

export interface TextAction {
  method: "GET" | "POST";
  path: string;
  description: string;
  requires?: string;
  fields?: string[];
  effect?: string;
}


export function preferredPageFormat(request: Request): PageFormatSelection {
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

export function respondPage(
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

export function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

export function textNavigationHint(selection: PageFormatSelection): string {
  const accept =
    selection.format === "markdown" ? "text/markdown" : "text/plain";
  const format = selection.format === "markdown" ? "md" : "text";
  return `GET paths below omit \`?format\`. Keep \`Accept: ${accept}\` to stay in text mode, or append \`?format=${format}\` when following a path without headers.`;
}

export function renderTextActions(actions: TextAction[]): string {
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

export function renderTextHints(hints: string[]): string {
  if (hints.length === 0) return "";
  return `\n## Hints\n${hints.map((hint) => `- ${hint}`).join("\n")}\n`;
}

export function authFooterHtml(): string {
  return `ripgit &mdash; <a href="https://github.com/deathbyknowledge/ripgit">open source</a> by <a href="https://x.com/caise_p">deathbyknowledge</a>`;
}

export function renderAuthPageHtml(options: {
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


/**
 * Redirect to a relative or absolute URL.
 * Response.redirect() only accepts absolute URLs in Cloudflare Workers,
 * so use this helper for any relative paths.
 */
export function redirect(location: string, status = 302): Response {
  return new Response(null, { status, headers: { Location: location } });
}
