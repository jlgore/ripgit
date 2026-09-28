//! Web UI: server-rendered HTML pages for browsing repositories.
//!
//! All HTML is generated from Rust using `format!()`. No build step,
//! no static assets, no framework. Highlight.js loaded from CDN for
//! syntax highlighting in the file viewer.

use crate::{
    api,
    authz::Role,
    diff,
    presentation::{self, Action, Hint, NegotiatedRepresentation},
    store,
};
use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag};
use worker::*;

mod actions;
mod commit;
mod home;
mod log;
mod search;
mod settings;
mod tree_blob;

pub(crate) use actions::{
    page_actions, page_actions_markdown, page_run, page_run_markdown, LogTail,
};
pub(crate) use commit::page_commit;
pub(crate) use commit::{page_commit_markdown, page_diff_markdown};
pub(crate) use home::{page_home, page_home_markdown};
pub(crate) use log::{page_log, page_log_markdown};
pub(crate) use search::{page_search, page_search_markdown};
pub(crate) use settings::{page_settings, page_settings_markdown};
pub(crate) use tree_blob::{page_blob, page_blob_markdown, page_tree, page_tree_markdown};

type Url = worker::Url;

// ---------------------------------------------------------------------------
// Layout: shared HTML shell
// ---------------------------------------------------------------------------

/// Who is looking at a page: their name, if signed in, and their role on the
/// repo. Pages use the role to decide which controls to show; the routes
/// behind those controls check it again.
#[derive(Clone, Copy)]
pub(crate) struct Viewer<'a> {
    pub name: Option<&'a str>,
    pub role: Role,
}

impl Viewer<'_> {
    pub fn can_write(&self) -> bool {
        self.role >= Role::Write
    }

    pub fn can_admin(&self) -> bool {
        self.role >= Role::Admin
    }
}

pub(crate) fn layout(
    title: &str,
    owner: &str,
    repo_name: &str,
    default_branch: &str,
    viewer: Viewer<'_>,
    content: &str,
) -> String {
    let is_owner = viewer.can_admin();
    let global_auth = match viewer.name {
        Some(name) => format!(
            r#"<a href="/{n}" class="nav-user">{n}</a><a href="/logout" class="nav-signout">Sign out</a>"#,
            n = html_escape(name),
        ),
        None => format!(
            r#"<a href="/login?next=/{o}/{r}/" class="nav-signin">Sign in</a>"#,
            o = html_escape(owner),
            r = html_escape(repo_name),
        ),
    };
    let repo_settings_link = if is_owner {
        format!(
            r#"<a href="/{}/{}/settings">Settings</a>"#,
            html_escape(owner),
            html_escape(repo_name)
        )
    } else {
        String::new()
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - {owner}/{repo_name} - ripgit</title>
<link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/styles/github.min.css">
<style>
{CSS}
</style>
</head>
<body>
<header>
  <div class="global-nav">
    <a href="/" class="logo">ripgit</a>
    <div class="global-auth">{global_auth}</div>
  </div>
  <div class="repo-bar-wrap">
    <div class="repo-bar">
      <div class="repo-crumb">
        <a href="/{owner}/" class="owner-name">{owner}</a>
        <span class="sep">/</span>
        <a href="/{owner}/{repo_name}/" class="repo-name">{repo_name}</a>
      </div>
      <div class="repo-search">
        <input type="text" id="nav-search-input" placeholder="Search..." autocomplete="off" spellcheck="false">
        <div id="nav-search-results" class="nav-search-results" hidden></div>
      </div>
      <nav class="repo-tabs">
        <a href="/{owner}/{repo_name}/">Code</a>
        <a href="/{owner}/{repo_name}/commits">Commits</a>
        <a href="/{owner}/{repo_name}/issues">Issues</a>
        <a href="/{owner}/{repo_name}/pulls">PRs</a>
        <a href="/{owner}/{repo_name}/actions">Actions</a>
        {repo_settings_link}
      </nav>
    </div>
  </div>
</header>
<main>
{content}
</main>
<script src="https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js"></script>
<script src="https://cdnjs.cloudflare.com/ajax/libs/highlightjs-line-numbers.js/2.8.0/highlightjs-line-numbers.min.js"></script>
<script>
hljs.highlightAll();
hljs.initLineNumbersOnLoad({{singleLine:true}});
setTimeout(function(){{
  var m=window.location.hash.match(/^#L(\d+)$/);
  if(m){{var td=document.querySelector('td[data-line-number="'+m[1]+'"]');
    if(td){{var tr=td.parentNode;tr.style.background='#fffbdd';tr.scrollIntoView({{block:'center'}})}}
  }}
}},150);
(function(){{
  var inp=document.getElementById('nav-search-input');
  var box=document.getElementById('nav-search-results');
  if(!inp||!box) return;
  var timer=null;
  var repoPath='{owner}/{repo_name}';
  var branch='{default_branch}';
  function esc(s){{return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');}}
  function scopeOf(q){{return /@(author|message):/.test(q)?'commits':'code';}}
  inp.addEventListener('input',function(){{
    clearTimeout(timer);
    var q=inp.value.trim();
    if(!q){{box.hidden=true;box.innerHTML='';return;}}
    timer=setTimeout(function(){{doSearch(q);}},200);
  }});
  inp.addEventListener('keydown',function(e){{
    if(e.key==='Enter'){{
      var q=inp.value.trim();
      if(q) window.location.href='/'+repoPath+'/search-ui?q='+encodeURIComponent(q)+'&scope='+scopeOf(q);
      e.preventDefault();
    }}else if(e.key==='Escape'){{
      box.hidden=true;inp.blur();
    }}
  }});
  document.addEventListener('click',function(e){{
    if(!inp.closest('.repo-search').contains(e.target)) box.hidden=true;
  }});
  function doSearch(q){{
    fetch('/'+repoPath+'/search?q='+encodeURIComponent(q)+'&scope='+scopeOf(q)+'&limit=10')
      .then(function(r){{return r.json();}})
      .then(function(data){{
        if(inp.value.trim()!==q) return;
        if(!data.results||!data.results.length){{
          box.innerHTML='<div class="nsr-empty">No results</div>';
          box.hidden=false;return;
        }}
        var html='';
        if(data.scope==='commits'){{
          data.results.forEach(function(r){{
            html+='<a class="nsr-item" href="/'+repoPath+'/commit/'+esc(r.hash)+'">'
              +'<div class="nsr-path">'+esc(r.hash.slice(0,7))+' \u2014 '+esc((r.message||'').split('\n')[0].slice(0,72))+'</div>'
              +'<div class="nsr-snippet">'+esc(r.author)+'</div>'
              +'</a>';
          }});
          html+='<a class="nsr-all" href="/'+repoPath+'/search-ui?q='+encodeURIComponent(q)+'&scope=commits">'
            +'View all \u2014 '+data.results.length+' commit'+(data.results.length===1?'':'s')
            +'</a>';
        }}else{{
          data.results.forEach(function(r){{
            var m=r.matches[0];
            var snippet=m?m.text.replace(/^\s+/,''):'';
            html+='<a class="nsr-item" href="/'+repoPath+'/blob/'+esc(branch)+'/'+esc(r.path)+(m?'#L'+m.line:'')+'">'
              +'<div class="nsr-path">'+esc(r.path)+'</div>'
              +(snippet?'<div class="nsr-snippet">'+esc(snippet)+'</div>':'')
              +'</a>';
          }});
          html+='<a class="nsr-all" href="/'+repoPath+'/search-ui?q='+encodeURIComponent(q)+'&scope=code">'
            +'View all \u2014 '+data.total_matches+' match'+(data.total_matches===1?'':'es')+' in '+data.total_files+' file'+(data.total_files===1?'':'s')
            +'</a>';
        }}
        box.innerHTML=html;box.hidden=false;
      }})
      .catch(function(){{}});
  }}
}})();
</script>
</body>
</html>"#,
        title = html_escape(title),
        owner = html_escape(owner),
        repo_name = html_escape(repo_name),
        default_branch = html_escape(default_branch),
        global_auth = global_auth,
        repo_settings_link = repo_settings_link,
        content = content,
        CSS = CSS,
    )
}

const CSS: &str = r#"
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
  font-size: 14px;
  color: #1f2328;
  background: #fff;
  line-height: 1.5;
}
a { color: #0969da; text-decoration: none; }
a:hover { text-decoration: underline; }
/* ── Global nav (identity only) ─────────────────────────────── */
.global-nav {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 24px;
  height: 44px;
}
header { border-bottom: 1px solid #d1d9e0; }
.logo { font-weight: 700; font-size: 15px; color: #1f2328; }
.global-auth { display: flex; align-items: center; gap: 10px; font-size: 13px; }

/* ── Repo bar (context + actions) ───────────────────────────── */
.repo-bar-wrap {
  border-top: 1px solid #d1d9e0;
  background: #f6f8fa;
}
.repo-bar {
  display: flex;
  align-items: center;
  gap: 16px;
  max-width: 1200px;
  margin: 0 auto;
  padding: 0 24px;
  height: 40px;
}
.repo-crumb { display: flex; align-items: center; gap: 4px; flex-shrink: 0; }
.repo-search { flex: 0 0 auto; }
.repo-tabs { margin-left: auto; display: flex; align-items: center; gap: 16px; flex-shrink: 0; }
.repo-tabs a { font-size: 13px; color: #656d76; }
.repo-tabs a:hover { color: #1f2328; text-decoration: none; }
.sep { color: #656d76; }
.owner-name { font-weight: 600; color: #1f2328; font-size: 13px; }
.repo-name { font-weight: 600; color: #1f2328; font-size: 13px; }
main {
  max-width: 1200px;
  margin: 0 auto;
  padding: 24px;
}
h1 { font-size: 20px; margin-bottom: 16px; }
h2 { font-size: 16px; margin-bottom: 12px; }

/* File tree */
.tree-table { width: 100%; border-collapse: collapse; }
.tree-table td {
  padding: 6px 12px;
  border-top: 1px solid #d1d9e0;
}
.tree-table tr:first-child td { border-top: none; }
.tree-icon { width: 20px; color: #656d76; }
.tree-name { }
.tree-msg { color: #656d76; text-align: right; }

/* Commit list */
.commit-list { list-style: none; }
.commit-item {
  padding: 8px 0;
  border-bottom: 1px solid #d1d9e0;
  display: flex;
  align-items: baseline;
  gap: 12px;
}
.commit-msg { flex: 1; }
.commit-msg a { color: #1f2328; }
.commit-hash {
  font-family: ui-monospace, SFMono-Regular, monospace;
  font-size: 12px;
  color: #0969da;
  background: #ddf4ff;
  padding: 2px 6px;
  border-radius: 4px;
}
.commit-time { color: #656d76; font-size: 12px; white-space: nowrap; }
.commit-author { color: #656d76; font-size: 12px; }

/* File viewer */
.file-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 8px 12px;
  background: #f6f8fa;
  border: 1px solid #d1d9e0;
  border-radius: 6px 6px 0 0;
}
.raw-btn {
  font-size: 12px;
  padding: 2px 8px;
  border: 1px solid #d1d9e0;
  border-radius: 4px;
  color: #1f2328;
  background: #fff;
  text-decoration: none;
}
.raw-btn:hover {
  background: #f6f8fa;
  text-decoration: none;
}
.nav-signin { color: #1f2328; border: 1px solid #d1d9e0; border-radius: 4px; padding: 3px 10px; text-decoration: none; background: #fff; }
.nav-signin:hover { background: #f6f8fa; }
.nav-user { font-weight: 600; color: #1f2328; text-decoration: none; }
.nav-signout { color: #656d76; text-decoration: none; }
.empty-repo { padding: 24px; background: #f6f8fa; border: 1px solid #d1d9e0; border-radius: 6px; }
.empty-repo h2 { margin-bottom: 12px; }
.empty-repo-msg { color: #656d76; }
.push-cmd { background: #fff; border: 1px solid #d1d9e0; border-radius: 6px; padding: 14px 16px; font-size: 13px; margin: 12px 0; overflow-x: auto; line-height: 1.6; }
.push-note { font-size: 13px; color: #656d76; margin-top: 8px; }
.push-note a { color: #0969da; }
.settings-section { border: 1px solid #d1d9e0; border-radius: 6px; padding: 20px 24px; margin-bottom: 20px; }
.settings-section h2 { font-size: 16px; margin-bottom: 10px; }
.settings-hint { font-size: 13px; color: #656d76; margin-bottom: 14px; }
.settings-danger { border-color: #cf222e; }
.settings-danger h2 { color: #cf222e; }
.stats-grid { display: flex; gap: 16px; flex-wrap: wrap; }
.stat-box { background: #f6f8fa; border: 1px solid #d1d9e0; border-radius: 6px; padding: 12px 20px; min-width: 100px; text-align: center; }
.stat-val { font-size: 22px; font-weight: 600; }
.stat-lbl { font-size: 12px; color: #656d76; margin-top: 2px; }
.action-row { display: flex; flex-direction: column; gap: 10px; }
.action-row form { display: flex; align-items: center; gap: 12px; }
.action-hint { font-size: 13px; color: #656d76; }
.btn-action { background: #f6f8fa; border: 1px solid #d1d9e0; border-radius: 6px; padding: 5px 14px; font-size: 13px; cursor: pointer; }
.btn-action:hover { background: #eaeef2; }
.btn-danger-action { background: #cf222e; color: #fff; border: none; border-radius: 6px; padding: 5px 14px; font-size: 13px; cursor: pointer; }
.btn-danger-action:hover { background: #a40e26; }
.inline-form { display: flex; gap: 8px; align-items: center; }
.branch-input { border: 1px solid #d1d9e0; border-radius: 6px; padding: 5px 10px; font-size: 13px; font-family: ui-monospace, monospace; width: 280px; }
.danger-confirm { width: 320px; }
.branch-input:focus { outline: none; border-color: #0969da; }
.file-content {
  border: 1px solid #d1d9e0;
  border-top: none;
  border-radius: 0 0 6px 6px;
  overflow-x: auto;
}
.file-content pre {
  margin: 0;
  padding: 12px;
  font-size: 13px;
  line-height: 1.45;
}
.file-content code { font-family: ui-monospace, SFMono-Regular, monospace; }
.line-numbers {
  display: inline-block;
  text-align: right;
  padding-right: 12px;
  margin-right: 12px;
  border-right: 1px solid #d1d9e0;
  color: #656d76;
  user-select: none;
  min-width: 40px;
}

/* Diff */
.diff-file {
  margin-bottom: 16px;
  border: 1px solid #d1d9e0;
  border-radius: 6px;
  overflow: hidden;
}
.diff-file-header {
  padding: 8px 12px;
  background: #f6f8fa;
  border-bottom: 1px solid #d1d9e0;
  font-family: ui-monospace, SFMono-Regular, monospace;
  font-size: 13px;
  display: flex;
  align-items: center;
  gap: 8px;
}
.diff-status {
  font-size: 11px;
  font-weight: 600;
  padding: 1px 6px;
  border-radius: 3px;
  text-transform: uppercase;
}
.diff-status.added { background: #dafbe1; color: #116329; }
.diff-status.deleted { background: #ffebe9; color: #82071e; }
.diff-status.modified { background: #ddf4ff; color: #0550ae; }
.diff-hunk-header {
  padding: 4px 12px;
  background: #ddf4ff;
  color: #656d76;
  font-family: ui-monospace, SFMono-Regular, monospace;
  font-size: 12px;
  border-top: 1px solid #d1d9e0;
}
.diff-table { width: 100%; border-collapse: collapse; font-size: 13px; }
.diff-table td {
  padding: 0 12px;
  font-family: ui-monospace, SFMono-Regular, monospace;
  white-space: pre-wrap;
  word-break: break-all;
  vertical-align: top;
  line-height: 20px;
}
.diff-ln {
  width: 1%;
  min-width: 40px;
  text-align: right;
  color: #656d76;
  user-select: none;
  padding: 0 8px;
}
.diff-line-add { background: #dafbe1; }
.diff-line-add .diff-ln { background: #ccffd8; }
.diff-line-del { background: #ffebe9; }
.diff-line-del .diff-ln { background: #ffd7d5; }
.diff-line-ctx { background: #fff; }

/* Stats */
.diff-stats {
  margin-bottom: 16px;
  padding: 12px;
  background: #f6f8fa;
  border: 1px solid #d1d9e0;
  border-radius: 6px;
  font-size: 13px;
}
.stat-add { color: #116329; font-weight: 600; }
.stat-del { color: #82071e; font-weight: 600; }

/* Search */
.search-form {
  display: flex;
  gap: 8px;
  margin-bottom: 16px;
}
.search-form input[type="text"] {
  flex: 1;
  padding: 8px 12px;
  border: 1px solid #d1d9e0;
  border-radius: 6px;
  font-size: 14px;
}
.search-form button {
  padding: 8px 16px;
  background: #2da44e;
  color: #fff;
  border: none;
  border-radius: 6px;
  cursor: pointer;
  font-size: 14px;
}
.search-result {
  padding: 12px;
  border: 1px solid #d1d9e0;
  border-radius: 6px;
  margin-bottom: 8px;
}
.search-result-path {
  font-family: ui-monospace, SFMono-Regular, monospace;
  font-size: 13px;
  margin-bottom: 4px;
}
.search-result-snippet {
  font-size: 13px;
  color: #656d76;
  white-space: pre-wrap;
}

/* Breadcrumb */
.breadcrumb {
  margin-bottom: 16px;
  font-size: 16px;
}
.breadcrumb a { font-weight: 600; }

/* README */
.readme-box {
  margin-top: 24px;
  border: 1px solid #d1d9e0;
  border-radius: 6px;
}
.readme-header {
  padding: 8px 12px;
  background: #f6f8fa;
  border-bottom: 1px solid #d1d9e0;
  font-weight: 600;
  font-size: 13px;
}
.readme-body {
  padding: 16px 24px;
  font-size: 14px;
  line-height: 1.6;
}
.readme-body pre {
  background: #f6f8fa;
  padding: 12px;
  border-radius: 6px;
  overflow-x: auto;
}

/* Pagination */
.pagination {
  display: flex;
  gap: 8px;
  margin-top: 16px;
}
.pagination a, .pagination span {
  padding: 6px 12px;
  border: 1px solid #d1d9e0;
  border-radius: 6px;
  font-size: 13px;
}
.pagination span { color: #656d76; }

/* Branch selector */
.branch-selector {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 16px;
}
.branch-selector select {
  padding: 5px 8px;
  border: 1px solid #d1d9e0;
  border-radius: 6px;
  background: #f6f8fa;
  font-size: 13px;
  font-weight: 600;
  cursor: pointer;
}
.branch-selector .branch-label {
  font-size: 13px;
  color: #656d76;
}

/* Line numbers (highlightjs-line-numbers.js overrides) */
.hljs-ln-numbers {
  text-align: right;
  padding-right: 12px !important;
  border-right: 1px solid #d1d9e0;
  color: #656d76;
  user-select: none;
  min-width: 40px;
  vertical-align: top;
}
.hljs-ln-code {
  padding-left: 12px !important;
}

/* Markdown */
.readme-body h1 { font-size: 24px; margin: 16px 0 8px; padding-bottom: 4px; border-bottom: 1px solid #d1d9e0; }
.readme-body h2 { font-size: 20px; margin: 14px 0 6px; padding-bottom: 4px; border-bottom: 1px solid #d1d9e0; }
.readme-body h3 { font-size: 16px; margin: 12px 0 4px; }
.readme-body p { margin: 8px 0; }
.readme-body ul, .readme-body ol { margin: 8px 0; padding-left: 24px; }
.readme-body li { margin: 2px 0; }
.readme-body code {
  font-family: ui-monospace, SFMono-Regular, monospace;
  font-size: 13px;
  background: #eff1f3;
  padding: 2px 6px;
  border-radius: 4px;
}
.readme-body pre code { background: none; padding: 0; }
.readme-body a { color: #0969da; }
.readme-body img { max-width: 100%; }
.readme-body hr { border: none; border-top: 1px solid #d1d9e0; margin: 16px 0; }
.readme-body blockquote {
  border-left: 3px solid #d1d9e0;
  padding-left: 12px;
  color: #656d76;
  margin: 8px 0;
}

/* Nav search */
.repo-search {
  position: relative;
}
.repo-search input {
  padding: 4px 10px;
  border: 1px solid #d1d9e0;
  border-radius: 6px;
  font-size: 13px;
  width: 220px;
  background: #fff;
  color: #1f2328;
}
.repo-search input:focus {
  outline: none;
  border-color: #0969da;
  box-shadow: 0 0 0 3px rgba(9,105,218,0.1);
}
/* Keep .nav-search as an alias so the JS selector still works */
.nav-search { position: relative; }
.nav-search-results {
  position: absolute;
  top: calc(100% + 6px);
  right: 0;
  width: 460px;
  background: #fff;
  border: 1px solid #d1d9e0;
  border-radius: 8px;
  box-shadow: 0 8px 24px rgba(0,0,0,0.12);
  z-index: 100;
  overflow: hidden;
  max-height: 460px;
  overflow-y: auto;
}
.nsr-item {
  display: block;
  padding: 8px 12px;
  border-bottom: 1px solid #f0f2f4;
  text-decoration: none;
  color: #1f2328;
}
.nsr-item:hover { background: #f6f8fa; text-decoration: none; }
.nsr-path {
  font-family: ui-monospace, SFMono-Regular, monospace;
  font-size: 12px;
  color: #0969da;
}
.nsr-snippet {
  font-family: ui-monospace, SFMono-Regular, monospace;
  font-size: 12px;
  color: #656d76;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  margin-top: 2px;
}
.nsr-empty {
  padding: 16px 12px;
  color: #656d76;
  font-size: 13px;
  text-align: center;
}
.nsr-all {
  display: block;
  padding: 8px 12px;
  font-size: 13px;
  color: #0969da;
  text-align: center;
  border-top: 1px solid #d1d9e0;
  background: #f6f8fa;
  text-decoration: none;
}
.nsr-all:hover { background: #eef1f5; text-decoration: none; }

/* ── Issues & PRs ───────────────────────────────────────────── */
.issue-list-header { display: flex; align-items: center; justify-content: space-between; margin-bottom: 12px; }
.issue-tabs { display: flex; gap: 0; border: 1px solid #d1d9e0; border-radius: 6px; overflow: hidden; margin-bottom: 0; }
.issue-tab { padding: 6px 16px; font-size: 13px; color: #656d76; background: #f6f8fa; border-right: 1px solid #d1d9e0; text-decoration: none; }
.issue-tab:last-child { border-right: none; }
.issue-tab.active { background: #fff; color: #1f2328; font-weight: 600; }
.issue-tab:hover { background: #eaeef2; text-decoration: none; }
.issue-list { border: 1px solid #d1d9e0; border-radius: 6px; overflow: hidden; margin-top: 8px; }
.issue-item { display: flex; align-items: flex-start; gap: 10px; padding: 10px 16px; border-bottom: 1px solid #d1d9e0; }
.issue-item:last-child { border-bottom: none; }
.issue-item:hover { background: #f6f8fa; }
.issue-state-icon { font-size: 14px; margin-top: 2px; flex-shrink: 0; }
.issue-state-icon.open { color: #1a7f37; }
.issue-state-icon.closed { color: #656d76; }
.issue-state-icon.merged { color: #8250df; }
.issue-item-main { flex: 1; min-width: 0; }
.issue-item-title { font-weight: 600; color: #1f2328; word-break: break-word; }
.issue-item-title:hover { color: #0969da; text-decoration: none; }
.issue-item-meta { font-size: 12px; color: #656d76; margin-top: 2px; }
.pr-branch-pair { font-size: 12px; color: #656d76; margin-left: 8px; }
.pr-branch-pair code { font-size: 12px; background: #f6f8fa; padding: 1px 5px; border-radius: 3px; border: 1px solid #d1d9e0; }
.issue-empty { padding: 24px; color: #656d76; text-align: center; }
.issue-badge { display: inline-flex; align-items: center; gap: 4px; padding: 3px 10px; border-radius: 20px; font-size: 12px; font-weight: 600; }
.issue-badge.open { background: #dafbe1; color: #1a7f37; }
.issue-badge.closed { background: #f0f2f4; color: #656d76; }
.issue-badge.merged { background: #eee4ff; color: #8250df; }
.issue-detail-header { margin-bottom: 20px; padding-bottom: 16px; border-bottom: 1px solid #d1d9e0; }
.issue-title-row h1 { font-size: 22px; margin-bottom: 8px; }
.issue-number-heading { color: #656d76; font-weight: 400; }
.issue-meta-row { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.issue-meta-text { font-size: 13px; color: #656d76; }
.issue-body-wrap { border: 1px solid #d1d9e0; border-radius: 6px; padding: 16px 20px; margin-bottom: 20px; }
.issue-body { line-height: 1.6; }
.issue-no-description { color: #656d76; font-style: italic; font-size: 14px; }
.comment { border: 1px solid #d1d9e0; border-radius: 6px; margin-bottom: 12px; overflow: hidden; }
.comment-header { display: flex; align-items: center; gap: 8px; padding: 8px 14px; background: #f6f8fa; border-bottom: 1px solid #d1d9e0; font-size: 13px; }
.comment-time { color: #656d76; font-size: 12px; }
.comment-body { padding: 12px 16px; font-size: 14px; line-height: 1.6; }
.comment-form-wrap { border: 1px solid #d1d9e0; border-radius: 6px; margin-top: 16px; overflow: hidden; }
.comment-header-row { padding: 8px 14px; background: #f6f8fa; border-bottom: 1px solid #d1d9e0; font-size: 13px; font-weight: 600; }
.comment-form { padding: 12px 16px; }
.comment-textarea { width: 100%; padding: 8px 12px; border: 1px solid #d1d9e0; border-radius: 6px; font-size: 14px; font-family: inherit; resize: vertical; }
.comment-textarea:focus { outline: none; border-color: #0969da; box-shadow: 0 0 0 3px rgba(9,105,218,0.1); }
.comment-form-footer { display: flex; justify-content: flex-end; gap: 8px; margin-top: 8px; }
.comment-thread { margin: 16px 0; }
.pr-diff-section { margin: 20px 0; }
.pr-diff-section h2 { font-size: 16px; margin-bottom: 10px; }
.pr-diff-stats { display: flex; gap: 12px; align-items: center; padding: 8px 14px; background: #f6f8fa; border: 1px solid #d1d9e0; border-radius: 6px; font-size: 13px; margin-bottom: 12px; }
.pr-merge-box { padding: 16px; background: #dafbe1; border: 1px solid #aef0be; border-radius: 6px; margin-bottom: 12px; display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.pr-merged-box { padding: 12px 16px; background: #eee4ff; border: 1px solid #d8b4fe; border-radius: 6px; margin-bottom: 12px; font-size: 13px; }
.pr-merge-hint { font-size: 12px; color: #1a7f37; margin: 0; }
.pr-branch-info { display: flex; align-items: center; gap: 8px; }
.branch-tag { background: #f6f8fa; border: 1px solid #d1d9e0; padding: 2px 8px; border-radius: 4px; font-size: 13px; }
.pr-branch-missing { color: #cf222e; font-size: 14px; }
.btn-primary { padding: 6px 16px; background: #2da44e; color: #fff; border: none; border-radius: 6px; font-size: 13px; cursor: pointer; text-decoration: none; display: inline-block; }
.btn-primary:hover { background: #2c974b; color: #fff; text-decoration: none; }
.btn-merge { padding: 8px 20px; background: #8250df; color: #fff; border: none; border-radius: 6px; font-size: 14px; cursor: pointer; font-weight: 600; }
.btn-merge:hover { background: #6e40c9; }
.new-issue-form { max-width: 760px; }
.form-group { margin-bottom: 16px; }
.form-label { display: block; font-weight: 600; font-size: 13px; margin-bottom: 6px; }
.form-hint { font-weight: 400; color: #656d76; }
.form-input { width: 100%; padding: 8px 12px; border: 1px solid #d1d9e0; border-radius: 6px; font-size: 14px; font-family: inherit; }
.form-input:focus { outline: none; border-color: #0969da; box-shadow: 0 0 0 3px rgba(9,105,218,0.1); }
.form-textarea { width: 100%; padding: 8px 12px; border: 1px solid #d1d9e0; border-radius: 6px; font-size: 14px; font-family: inherit; resize: vertical; }
.form-textarea:focus { outline: none; border-color: #0969da; box-shadow: 0 0 0 3px rgba(9,105,218,0.1); }
.form-actions { display: flex; gap: 8px; align-items: center; }
.form-branch-row { display: flex; align-items: flex-end; gap: 12px; flex-wrap: wrap; }
.form-branch-row .arrow { font-size: 18px; color: #656d76; padding-bottom: 6px; }
"#;

// ---------------------------------------------------------------------------
// Page: Owner profile (/:owner/)
// Handled at the Worker level since there is no per-owner DO.
// ---------------------------------------------------------------------------

pub fn page_owner_profile(
    owner: &str,
    viewer: Viewer<'_>,
    url: &Url,
    repos: &[String],
) -> Result<Response> {
    let is_owner = viewer.can_admin();
    let host = url.host_str().unwrap_or("your-worker.dev");
    let scheme = url.scheme();

    // Repo list (shown to everyone, empty state differs for owner vs visitor)
    let repo_list = if repos.is_empty() {
        if is_owner {
            format!(
                r#"<div class="push-box">
  <h2>Create your first repo</h2>
  <p>Repos are created on first push. Pick any name:</p>
  <pre class="push-cmd">cd my-project
git init
git add .
git commit -m "initial commit"
git remote add origin {scheme}://{owner}:TOKEN@{host}/{owner}/my-project
git push origin main</pre>
  <p class="push-note">Get a <code>TOKEN</code> from <a href="/settings">Settings → Access Tokens</a>.</p>
</div>"#,
                owner = html_escape(owner),
                scheme = scheme,
                host = host,
            )
        } else {
            r#"<p class="muted">No public repositories.</p>"#.to_string()
        }
    } else {
        let mut items = String::new();
        for repo in repos {
            items.push_str(&format!(
                r#"<li class="repo-item">
  <a href="/{owner}/{repo}" class="repo-link">{repo}</a>
</li>"#,
                owner = html_escape(owner),
                repo = html_escape(repo),
            ));
        }
        let mut html = format!(r#"<ul class="repo-list">{}</ul>"#, items);
        if is_owner {
            html.push_str(&format!(
                r#"<details class="push-box push-box-collapsed">
  <summary>Push a new repo</summary>
  <pre class="push-cmd" style="margin-top:12px">cd my-project
git init &amp;&amp; git add . &amp;&amp; git commit -m "initial commit"
git remote add origin {scheme}://{owner}:TOKEN@{host}/{owner}/my-project
git push origin main</pre>
  <p class="push-note">Get a <code>TOKEN</code> from <a href="/settings">Settings</a>.</p>
</details>"#,
                owner = html_escape(owner),
                scheme = scheme,
                host = host,
            ));
        }
        html
    };

    let body = format!(
        r#"<div class="profile-wrap">
<div class="profile-header">
  <h1 class="profile-title">{owner}</h1>
  {settings_link}
</div>
{repo_list}
</div>"#,
        owner = html_escape(owner),
        settings_link = if is_owner {
            r#"<span><a href="/settings/artifacts" class="profile-settings">Artifacts</a> · <a href="/settings" class="profile-settings">Settings</a></span>"#.to_string()
        } else {
            String::new()
        },
        repo_list = repo_list,
    );

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{owner} — ripgit</title>
<style>
  *{{margin:0;padding:0;box-sizing:border-box}}
  body{{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;font-size:14px;color:#1f2328;background:#fff}}
  .topbar{{background:#fff;border-bottom:1px solid #d1d9e0;padding:12px 24px;display:flex;align-items:center;gap:8px}}
  .topbar a{{color:#1f2328;text-decoration:none;font-weight:600}}
  .topbar .sep{{color:#656d76}}
  .profile-wrap{{max-width:800px;margin:40px auto;padding:0 24px}}
  .profile-header{{display:flex;align-items:baseline;justify-content:space-between;margin-bottom:24px}}
  .profile-title{{font-size:28px}}
  .profile-settings{{color:#656d76;font-size:13px;text-decoration:none}}
  .profile-settings:hover{{color:#0969da}}
  .repo-list{{list-style:none;border:1px solid #d1d9e0;border-radius:6px;overflow:hidden}}
  .repo-item{{border-bottom:1px solid #d1d9e0;padding:12px 16px}}
  .repo-item:last-child{{border-bottom:none}}
  .repo-link{{font-weight:600;color:#0969da;text-decoration:none;font-size:15px}}
  .repo-link:hover{{text-decoration:underline}}
  .push-box{{margin-top:20px;background:#f6f8fa;border:1px solid #d1d9e0;border-radius:6px;padding:20px}}
  .push-box-collapsed{{cursor:pointer}}
  .push-box-collapsed summary{{font-size:13px;color:#656d76;font-weight:600}}
  .push-box h2{{font-size:16px;margin-bottom:8px}}
  .push-cmd{{background:#fff;border:1px solid #d1d9e0;border-radius:6px;padding:14px 16px;font-size:13px;margin:12px 0;overflow-x:auto;line-height:1.6;font-family:ui-monospace,monospace}}
  .push-note{{font-size:13px;color:#656d76;margin-top:6px}}
  .push-note a{{color:#0969da}}
  .muted{{color:#656d76}}
  code{{background:#f6f8fa;border:1px solid #d1d9e0;border-radius:3px;padding:1px 5px;font-size:12px;font-family:ui-monospace,monospace}}
  p{{line-height:1.5}}
</style>
</head>
<body>
<div class="topbar">
  <a href="/">ripgit</a>
  <span class="sep">/</span>
  <span>{owner}</span>
</div>
{body}
</body>
</html>"#,
        owner = html_escape(owner),
        body = body,
    );

    let mut resp = Response::from_bytes(html.into_bytes())?;
    resp.headers_mut()
        .set("Content-Type", "text/html; charset=utf-8")?;
    Ok(resp)
}

pub fn page_owner_profile_markdown(
    owner: &str,
    viewer: Viewer<'_>,
    url: &Url,
    repos: &[String],
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let is_owner = viewer.can_admin();
    let host = url.host_str().unwrap_or("your-worker.dev");
    let scheme = url.scheme();

    let mut markdown = format!("# {}\n\nRepositories: `{}`\n", owner, repos.len());

    if repos.is_empty() {
        if is_owner {
            markdown.push_str("\nNo repositories yet. Repositories are created on first push.\n");
            markdown.push_str("\n## First Push\n\n```bash\n");
            markdown.push_str("cd my-project\n");
            markdown.push_str("git init\n");
            markdown.push_str("git add .\n");
            markdown.push_str("git commit -m \"initial commit\"\n");
            markdown.push_str(&format!(
                "git remote add origin {}://{}:TOKEN@{}/{}/my-project\n",
                scheme, owner, host, owner
            ));
            markdown.push_str("git push origin main\n");
            markdown.push_str("```\n");
        } else {
            markdown.push_str("\nNo public repositories.\n");
        }
    } else {
        markdown.push_str("\n## Repositories (GET paths)\n");
        for repo in repos {
            markdown.push_str(&format!("- `{}` - `/{}/{}`\n", repo, owner, repo));
        }

        if is_owner {
            markdown.push_str("\n## Push a New Repository\n\n```bash\n");
            markdown.push_str("cd my-project\n");
            markdown.push_str("git init\n");
            markdown.push_str("git add .\n");
            markdown.push_str("git commit -m \"initial commit\"\n");
            markdown.push_str(&format!(
                "git remote add origin {}://{}:TOKEN@{}/{}/my-project\n",
                scheme, owner, host, owner
            ));
            markdown.push_str("git push origin main\n");
            markdown.push_str("```\n");
        }
    }

    let mut actions = vec![Action::get(
        format!("/{}/", owner),
        "reload this owner profile",
    )];
    if is_owner {
        actions.push(Action::get("/settings", "open account settings"));
    }

    let hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new("Repository paths listed above are GET routes under this owner."),
        Hint::new(
            "Repositories are created on first push; there is no separate create-repo endpoint.",
        ),
    ];

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    presentation::markdown_response(&markdown, selection)
}

// ---------------------------------------------------------------------------
// Raw file serving
// ---------------------------------------------------------------------------

pub fn serve_raw(sql: &SqlStorage, ref_name: &str, path: &str) -> Result<Response> {
    if path.is_empty() {
        return Response::error("path required", 400);
    }

    let commit_hash = match api::resolve_ref(sql, ref_name)? {
        Some(h) => h,
        None => return Response::error("ref not found", 404),
    };

    let blob_hash = match resolve_path_to_blob(sql, &commit_hash, path) {
        Ok(h) => h,
        Err(_) => return Response::error("Not Found", 404),
    };

    let content = load_blob(sql, &blob_hash)?;
    let filename = path.rsplit('/').next().unwrap_or(path);
    let content_type = raw_content_type(filename, &content);

    let mut resp = Response::from_bytes(content)?;
    resp.headers_mut().set("Content-Type", content_type)?;
    // Prevent the browser from sniffing or executing the content
    resp.headers_mut()
        .set("X-Content-Type-Options", "nosniff")?;
    resp.headers_mut()
        .set("Content-Security-Policy", "default-src 'none'")?;
    Ok(resp)
}

/// Pick a safe Content-Type for raw file serving.
/// Text files are served as text/plain (never text/html) so the browser
/// displays them rather than executing anything. Images get their proper type
/// so they can be embedded/viewed directly. Everything else that contains
/// null bytes is treated as binary.
fn raw_content_type(filename: &str, content: &[u8]) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        // SVG/HTML/XML/JSON: always text/plain — never execute in browser
        _ => {
            let is_binary = !content.is_empty() && content[..content.len().min(8192)].contains(&0);
            if is_binary {
                "application/octet-stream"
            } else {
                "text/plain; charset=utf-8"
            }
        }
    }
}

struct CommitListItem {
    hash: String,
    short_hash: String,
    subject: String,
    author: String,
    relative_time: String,
}

fn summarize_commits(commits: Vec<CommitMeta>) -> Vec<CommitListItem> {
    commits
        .into_iter()
        .map(|commit| CommitListItem {
            short_hash: commit.hash[..7.min(commit.hash.len())].to_string(),
            subject: first_line(&commit.message),
            author: commit.author,
            relative_time: format_time(commit.commit_time),
            hash: commit.hash,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Diff rendering
// ---------------------------------------------------------------------------

pub(crate) fn render_file_diff(file: &diff::FileDiff) -> String {
    let status_class = match file.status {
        diff::DiffStatus::Added => "added",
        diff::DiffStatus::Deleted => "deleted",
        diff::DiffStatus::Modified => "modified",
    };
    let status_label = match file.status {
        diff::DiffStatus::Added => "A",
        diff::DiffStatus::Deleted => "D",
        diff::DiffStatus::Modified => "M",
    };

    let mut html = String::new();
    html.push_str(r#"<div class="diff-file">"#);
    html.push_str(&format!(
        r#"<div class="diff-file-header"><span class="diff-status {cls}">{label}</span> {path}</div>"#,
        cls = status_class,
        label = status_label,
        path = html_escape(&file.path),
    ));

    if let Some(hunks) = &file.hunks {
        for hunk in hunks {
            // Check for binary marker
            if hunk.lines.len() == 1 && hunk.lines[0].tag == "binary" {
                html.push_str(r#"<div class="diff-hunk-header">Binary files differ</div>"#);
                continue;
            }

            html.push_str(&format!(
                r#"<div class="diff-hunk-header">@@ -{},{} +{},{} @@</div>"#,
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count,
            ));

            html.push_str(r#"<table class="diff-table">"#);
            let mut old_ln = hunk.old_start;
            let mut new_ln = hunk.new_start;

            for line in &hunk.lines {
                let (class, prefix, oln, nln) = match line.tag {
                    "add" => {
                        let n = new_ln;
                        new_ln += 1;
                        ("diff-line-add", "+", String::new(), n.to_string())
                    }
                    "delete" => {
                        let o = old_ln;
                        old_ln += 1;
                        ("diff-line-del", "-", o.to_string(), String::new())
                    }
                    _ => {
                        let o = old_ln;
                        let n = new_ln;
                        old_ln += 1;
                        new_ln += 1;
                        ("diff-line-ctx", " ", o.to_string(), n.to_string())
                    }
                };

                // Strip trailing newline for display
                let content = line.content.trim_end_matches('\n');

                html.push_str(&format!(
                    r#"<tr class="{cls}"><td class="diff-ln">{oln}</td><td class="diff-ln">{nln}</td><td>{prefix}{content}</td></tr>"#,
                    cls = class,
                    oln = oln,
                    nln = nln,
                    prefix = prefix,
                    content = html_escape(content),
                ));
            }
            html.push_str("</table>");
        }
    }

    html.push_str("</div>");
    html
}

// ---------------------------------------------------------------------------
// Commit list rendering (shared by home + log)
// ---------------------------------------------------------------------------

fn render_commit_list(
    commits: &[CommitListItem],
    owner: &str,
    repo_name: &str,
    show_author: bool,
) -> String {
    let mut html = String::new();
    html.push_str(r#"<ul class="commit-list">"#);
    for commit in commits {
        let commit_path = format!("/{}/{}/commit/{}", owner, repo_name, commit.hash);
        html.push_str(&format!(
            r#"<li class="commit-item">
  <a class="commit-hash" href="{commit_path}">{short}</a>
  <span class="commit-msg"><a href="{commit_path}">{msg}</a></span>
  {author}
  <span class="commit-time">{time}</span>
</li>"#,
            commit_path = html_escape(&commit_path),
            short = html_escape(&commit.short_hash),
            msg = html_escape(&commit.subject),
            author = if !show_author || commit.author.is_empty() {
                String::new()
            } else {
                format!(
                    r#"<span class="commit-author">{}</span>"#,
                    html_escape(&commit.author)
                )
            },
            time = html_escape(&commit.relative_time),
        ));
    }
    html.push_str("</ul>");
    html
}

// ---------------------------------------------------------------------------
// Data loading helpers
// ---------------------------------------------------------------------------

struct SortedEntry {
    name: String,
    hash: String,
    is_tree: bool,
}

fn load_sorted_tree(sql: &SqlStorage, tree_hash: &str) -> Result<Vec<SortedEntry>> {
    let entries = store::load_tree_from_db(sql, tree_hash)?;

    let mut dirs: Vec<SortedEntry> = Vec::new();
    let mut files: Vec<SortedEntry> = Vec::new();

    for e in entries {
        let se = SortedEntry {
            name: e.name,
            hash: e.hash,
            is_tree: e.mode == 0o040000,
        };
        if se.is_tree {
            dirs.push(se);
        } else {
            files.push(se);
        }
    }

    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    dirs.append(&mut files);
    Ok(dirs)
}

fn load_tree_for_commit(sql: &SqlStorage, commit_hash: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Row {
        tree_hash: String,
    }

    let rows: Vec<Row> = sql
        .exec(
            "SELECT tree_hash FROM commits WHERE hash = ?",
            vec![SqlStorageValue::from(commit_hash.to_string())],
        )?
        .to_array()?;

    rows.into_iter()
        .next()
        .map(|r| r.tree_hash)
        .ok_or_else(|| Error::RustError(format!("commit not found: {}", commit_hash)))
}

fn resolve_path_to_tree(sql: &SqlStorage, commit_hash: &str, path: &str) -> Result<String> {
    let root = load_tree_for_commit(sql, commit_hash)?;
    if path.is_empty() {
        return Ok(root);
    }

    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut current = root;

    for segment in &segments {
        let entry = find_tree_entry(sql, &current, segment)?;
        match entry {
            Some((hash, mode)) if mode == 0o040000 => {
                current = hash;
            }
            Some(_) => {
                return Err(Error::RustError(format!(
                    "'{}' is not a directory",
                    segment
                )));
            }
            None => {
                return Err(Error::RustError(format!("'{}' not found in tree", segment)));
            }
        }
    }

    Ok(current)
}

fn resolve_path_to_blob(sql: &SqlStorage, commit_hash: &str, path: &str) -> Result<String> {
    let root = load_tree_for_commit(sql, commit_hash)?;
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if segments.is_empty() {
        return Err(Error::RustError("empty path".into()));
    }

    let mut current_tree = root;

    for (i, segment) in segments.iter().enumerate() {
        let entry = find_tree_entry(sql, &current_tree, segment)?;
        match entry {
            Some((hash, mode)) => {
                if i < segments.len() - 1 {
                    if mode != 0o040000 {
                        return Err(Error::RustError(format!(
                            "'{}' is not a directory",
                            segment
                        )));
                    }
                    current_tree = hash;
                } else {
                    // Last segment — should be a blob
                    return Ok(hash);
                }
            }
            None => {
                return Err(Error::RustError(format!("'{}' not found", segment)));
            }
        }
    }

    Err(Error::RustError("path resolution failed".into()))
}

fn find_tree_entry(sql: &SqlStorage, tree_hash: &str, name: &str) -> Result<Option<(String, u32)>> {
    #[derive(serde::Deserialize)]
    struct Row {
        entry_hash: String,
        mode: i64,
    }

    let rows: Vec<Row> = sql
        .exec(
            "SELECT entry_hash, mode FROM trees WHERE tree_hash = ? AND name = ?",
            vec![
                SqlStorageValue::from(tree_hash.to_string()),
                SqlStorageValue::from(name.to_string()),
            ],
        )?
        .to_array()?;

    Ok(rows
        .into_iter()
        .next()
        .map(|r| (r.entry_hash, r.mode as u32)))
}

fn load_blob(sql: &SqlStorage, blob_hash: &str) -> Result<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct BlobInfo {
        group_id: i64,
        version_in_group: i64,
    }

    let rows: Vec<BlobInfo> = sql
        .exec(
            "SELECT group_id, version_in_group FROM blobs WHERE blob_hash = ?",
            vec![SqlStorageValue::from(blob_hash.to_string())],
        )?
        .to_array()?;

    match rows.into_iter().next() {
        Some(info) => store::reconstruct_blob(sql, info.group_id, info.version_in_group),
        None => Err(Error::RustError(format!("blob not found: {}", blob_hash))),
    }
}

struct CommitMeta {
    hash: String,
    author: String,
    author_email: String,
    commit_time: i64,
    message: String,
}

fn load_commit_meta(sql: &SqlStorage, hash: &str) -> Result<CommitMeta> {
    #[derive(serde::Deserialize)]
    struct Row {
        author: String,
        author_email: String,
        commit_time: i64,
        message: String,
    }

    let rows: Vec<Row> = sql
        .exec(
            "SELECT author, author_email, commit_time, message
             FROM commits WHERE hash = ?",
            vec![SqlStorageValue::from(hash.to_string())],
        )?
        .to_array()?;

    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| Error::RustError(format!("commit not found: {}", hash)))?;

    Ok(CommitMeta {
        hash: hash.to_string(),
        author: row.author,
        author_email: row.author_email,
        commit_time: row.commit_time,
        message: row.message,
    })
}

fn walk_commits(sql: &SqlStorage, head: &str, limit: i64, offset: i64) -> Result<Vec<CommitMeta>> {
    let mut result = Vec::new();
    let mut current = Some(head.to_string());
    let mut skipped = 0i64;

    while let Some(hash) = current {
        if result.len() as i64 >= limit {
            break;
        }

        let meta = match load_commit_meta_opt(sql, &hash)? {
            Some(m) => m,
            None => break,
        };

        // Get first parent for next iteration
        #[derive(serde::Deserialize)]
        struct ParentRow {
            parent_hash: String,
        }
        let parents: Vec<ParentRow> = sql
            .exec(
                "SELECT parent_hash FROM commit_parents
                 WHERE commit_hash = ? ORDER BY ordinal ASC LIMIT 1",
                vec![SqlStorageValue::from(hash.clone())],
            )?
            .to_array()?;

        current = parents.into_iter().next().map(|p| p.parent_hash);

        if skipped < offset {
            skipped += 1;
            continue;
        }

        result.push(meta);
    }

    Ok(result)
}

fn load_commit_meta_opt(sql: &SqlStorage, hash: &str) -> Result<Option<CommitMeta>> {
    #[derive(serde::Deserialize)]
    struct Row {
        author: String,
        author_email: String,
        commit_time: i64,
        message: String,
    }

    let rows: Vec<Row> = sql
        .exec(
            "SELECT author, author_email, commit_time, message
             FROM commits WHERE hash = ?",
            vec![SqlStorageValue::from(hash.to_string())],
        )?
        .to_array()?;

    Ok(rows.into_iter().next().map(|row| CommitMeta {
        hash: hash.to_string(),
        author: row.author,
        author_email: row.author_email,
        commit_time: row.commit_time,
        message: row.message,
    }))
}

pub(crate) fn resolve_default_branch(sql: &SqlStorage) -> Result<(String, Option<String>)> {
    // Try main first, then the first ref we find
    if let Some(hash) = api::resolve_ref(sql, "main")? {
        return Ok(("main".to_string(), Some(hash)));
    }
    if let Some(hash) = api::resolve_ref(sql, "master")? {
        return Ok(("master".to_string(), Some(hash)));
    }

    // Fall back to first branch
    #[derive(serde::Deserialize)]
    struct Row {
        name: String,
        commit_hash: String,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT name, commit_hash FROM refs
             WHERE name LIKE 'refs/heads/%' LIMIT 1",
            None,
        )?
        .to_array()?;

    match rows.into_iter().next() {
        Some(r) => {
            let branch = r
                .name
                .strip_prefix("refs/heads/")
                .unwrap_or(&r.name)
                .to_string();
            Ok((branch, Some(r.commit_hash)))
        }
        None => Ok(("main".to_string(), None)),
    }
}

fn load_branches(sql: &SqlStorage) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Row {
        name: String,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT name FROM refs WHERE name LIKE 'refs/heads/%' ORDER BY name",
            None,
        )?
        .to_array()?;
    Ok(rows
        .into_iter()
        .map(|r| {
            r.name
                .strip_prefix("refs/heads/")
                .unwrap_or(&r.name)
                .to_string()
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Markdown renderer
// ---------------------------------------------------------------------------

pub(crate) fn render_markdown(text: &str) -> String {
    render_markdown_with_context(text, None)
}

pub(crate) fn render_repo_markdown(
    sql: &SqlStorage,
    text: &str,
    owner: &str,
    repo_name: &str,
    ref_name: &str,
    commit_hash: &str,
    source_path: &str,
) -> String {
    let context = RepoMarkdownContext {
        sql,
        owner,
        repo_name,
        ref_name,
        commit_hash,
        source_path,
    };
    render_markdown_with_context(text, Some(&context))
}

struct RepoMarkdownContext<'a> {
    sql: &'a SqlStorage,
    owner: &'a str,
    repo_name: &'a str,
    ref_name: &'a str,
    commit_hash: &'a str,
    source_path: &'a str,
}

enum RepoMarkdownTargetKind {
    Tree,
    Blob,
}

fn render_markdown_with_context(text: &str, context: Option<&RepoMarkdownContext<'_>>) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);

    let parser =
        Parser::new_ext(text, options).map(|event| sanitize_markdown_event(event, context));
    let mut output = String::new();
    html::push_html(&mut output, parser);
    output
}

fn sanitize_markdown_event<'a>(
    event: Event<'a>,
    context: Option<&RepoMarkdownContext<'_>>,
) -> Event<'a> {
    match event {
        Event::Start(tag) => Event::Start(sanitize_markdown_tag(tag, context)),
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        _ => event,
    }
}

fn sanitize_markdown_tag<'a>(tag: Tag<'a>, context: Option<&RepoMarkdownContext<'_>>) -> Tag<'a> {
    match tag {
        Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        } => Tag::Link {
            link_type,
            dest_url: sanitize_markdown_url(rewrite_repo_markdown_url(dest_url, false, context)),
            title,
            id,
        },
        Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        } => Tag::Image {
            link_type,
            dest_url: sanitize_markdown_url(rewrite_repo_markdown_url(dest_url, true, context)),
            title,
            id,
        },
        _ => tag,
    }
}

fn rewrite_repo_markdown_url<'a>(
    url: CowStr<'a>,
    is_image: bool,
    context: Option<&RepoMarkdownContext<'_>>,
) -> CowStr<'a> {
    let Some(context) = context else {
        return url;
    };

    let raw = url.as_ref().trim();
    if !is_repo_relative_markdown_url(raw) {
        return url;
    }

    let (path_part, suffix) = split_markdown_destination(raw);
    let normalized = normalize_repo_relative_path(context.source_path, path_part);

    if normalized.is_empty() {
        return CowStr::from(format!(
            "/{}/{}/?ref={}{}",
            context.owner, context.repo_name, context.ref_name, suffix
        ));
    }

    let base = if is_image {
        format!(
            "/{}/{}/raw/{}/{}",
            context.owner, context.repo_name, context.ref_name, normalized
        )
    } else {
        match repo_markdown_target_kind(context.sql, context.commit_hash, &normalized) {
            Ok(Some(RepoMarkdownTargetKind::Tree)) => format!(
                "/{}/{}/tree/{}/{}",
                context.owner, context.repo_name, context.ref_name, normalized
            ),
            Ok(Some(RepoMarkdownTargetKind::Blob)) | Ok(None) | Err(_) => format!(
                "/{}/{}/blob/{}/{}",
                context.owner, context.repo_name, context.ref_name, normalized
            ),
        }
    };

    CowStr::from(format!("{}{}", base, suffix))
}

fn is_repo_relative_markdown_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with('/')
        || trimmed.starts_with('?')
    {
        return false;
    }

    let Some((scheme, _)) = trimmed.split_once(':') else {
        return true;
    };

    !scheme
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
}

fn split_markdown_destination(url: &str) -> (&str, &str) {
    for (idx, ch) in url.char_indices() {
        if matches!(ch, '?' | '#') {
            return (&url[..idx], &url[idx..]);
        }
    }
    (url, "")
}

fn normalize_repo_relative_path(source_path: &str, target: &str) -> String {
    let base_dir = source_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    let mut segments: Vec<&str> = if target.starts_with('/') || base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect()
    };

    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }

    segments.join("/")
}

fn repo_markdown_target_kind(
    sql: &SqlStorage,
    commit_hash: &str,
    path: &str,
) -> Result<Option<RepoMarkdownTargetKind>> {
    if path.is_empty() {
        return Ok(Some(RepoMarkdownTargetKind::Tree));
    }

    let mut current_tree = load_tree_for_commit(sql, commit_hash)?;
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    for (idx, segment) in segments.iter().enumerate() {
        let Some((hash, mode)) = find_tree_entry(sql, &current_tree, segment)? else {
            return Ok(None);
        };
        let is_last = idx == segments.len() - 1;

        if is_last {
            return Ok(Some(if mode == 0o040000 {
                RepoMarkdownTargetKind::Tree
            } else {
                RepoMarkdownTargetKind::Blob
            }));
        }

        if mode != 0o040000 {
            return Ok(None);
        }
        current_tree = hash;
    }

    Ok(None)
}

fn sanitize_markdown_url(url: CowStr<'_>) -> CowStr<'_> {
    if is_safe_markdown_url(url.as_ref()) {
        url
    } else {
        CowStr::from("#")
    }
}

fn is_safe_markdown_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return true;
    }

    if trimmed.starts_with('#')
        || trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.starts_with('?')
    {
        return true;
    }

    let Some((scheme, _)) = trimmed.split_once(':') else {
        return true;
    };

    if !scheme
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
    {
        return true;
    }

    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "mailto"
    )
}

#[cfg(test)]
mod markdown_tests {
    use super::{
        is_repo_relative_markdown_url, normalize_repo_relative_path, render_markdown,
        split_markdown_destination,
    };

    #[test]
    fn render_markdown_preserves_unicode() {
        let html = render_markdown("café 😅 — 你好");

        assert!(html.contains("café 😅 — 你好"));
    }

    #[test]
    fn render_markdown_supports_common_gfm_features() {
        let html = render_markdown(
            "| a | b |\n| - | - |\n| 1 | 2 |\n\n- [x] done\n- [ ] todo\n\n```rust\nfn main() {}\n```",
        );

        assert!(html.contains("<table>"));
        assert!(html.contains("checkbox"));
        assert!(html.contains("language-rust"));
    }

    #[test]
    fn render_markdown_escapes_raw_html_and_unsafe_links() {
        let html = render_markdown("<script>alert(1)</script>\n\n[bad](javascript:alert(1))");

        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("href=\"#\""));
        assert!(!html.contains("javascript:alert(1)"));
    }

    #[test]
    fn normalize_repo_relative_path_resolves_dot_segments() {
        assert_eq!(
            normalize_repo_relative_path("docs/README.md", "../images/logo.png"),
            "images/logo.png"
        );
        assert_eq!(
            normalize_repo_relative_path("README.md", "./guides/setup.md"),
            "guides/setup.md"
        );
    }

    #[test]
    fn split_markdown_destination_preserves_query_and_fragment() {
        assert_eq!(
            split_markdown_destination("docs/setup.md?view=1#intro"),
            ("docs/setup.md", "?view=1#intro")
        );
        assert_eq!(
            split_markdown_destination("docs/setup.md"),
            ("docs/setup.md", "")
        );
    }

    #[test]
    fn relative_url_detection_ignores_absolute_targets() {
        assert!(is_repo_relative_markdown_url("docs/setup.md"));
        assert!(!is_repo_relative_markdown_url("https://example.com"));
        assert!(!is_repo_relative_markdown_url("mailto:test@example.com"));
        assert!(!is_repo_relative_markdown_url("#usage"));
        assert!(!is_repo_relative_markdown_url("/docs/setup.md"));
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(crate) fn html_response(body: &str) -> Result<Response> {
    let mut resp = Response::from_html(body)?;
    resp.headers_mut().set("Cache-Control", "no-cache")?;
    Ok(resp)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

pub(crate) fn format_time(unix: i64) -> String {
    // Simple relative time
    let now = js_sys_date_now() / 1000.0;
    let diff = (now as i64) - unix;

    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        let m = diff / 60;
        format!("{} minute{} ago", m, if m == 1 { "" } else { "s" })
    } else if diff < 86400 {
        let h = diff / 3600;
        format!("{} hour{} ago", h, if h == 1 { "" } else { "s" })
    } else if diff < 2592000 {
        let d = diff / 86400;
        format!("{} day{} ago", d, if d == 1 { "" } else { "s" })
    } else if diff < 31536000 {
        let m = diff / 2592000;
        format!("{} month{} ago", m, if m == 1 { "" } else { "s" })
    } else {
        let y = diff / 31536000;
        format!("{} year{} ago", y, if y == 1 { "" } else { "s" })
    }
}

fn js_sys_date_now() -> f64 {
    // In Cloudflare Workers, Date.now() returns the time at request start.
    // We can't use js_sys directly without adding it as a dependency.
    // Instead, use worker::Date.
    worker::Date::now().as_millis() as f64
}
