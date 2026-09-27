use super::*;

struct SearchPage {
    route_path: String,
    owner: String,
    repo_name: String,
    default_branch: String,
    raw_query: String,
    effective_query: String,
    requested_scope: String,
    scope: String,
    path_filter: Option<String>,
    ext_filter: Option<String>,
    scope_inferred: bool,
    state: SearchPageState,
}

enum SearchPageState {
    Idle,
    Code {
        results: Vec<store::CodeSearchResult>,
        total_matches: usize,
    },
    Commits {
        results: Vec<store::CommitSearchResult>,
    },
}

impl SearchPage {
    fn current_path(&self) -> String {
        self.search_ui_path(&self.scope)
    }

    fn search_ui_path(&self, scope: &str) -> String {
        build_query_path(
            &self.route_path,
            &[
                (
                    "q",
                    (!self.raw_query.is_empty()).then_some(self.raw_query.as_str()),
                ),
                ("scope", Some(scope)),
            ],
        )
    }

    fn json_search_path(&self) -> String {
        build_query_path(
            &format!("/{}/{}/search", self.owner, self.repo_name),
            &[
                (
                    "q",
                    (!self.raw_query.is_empty()).then_some(self.raw_query.as_str()),
                ),
                ("scope", Some(self.scope.as_str())),
            ],
        )
    }

    fn commit_path(&self, hash: &str) -> String {
        format!("/{}/{}/commit/{}", self.owner, self.repo_name, hash)
    }

    fn blob_path(&self, path: &str) -> String {
        format!(
            "/{}/{}/blob/{}/{}",
            self.owner, self.repo_name, self.default_branch, path
        )
    }

    fn blob_line_path(&self, path: &str, line_number: usize) -> String {
        format!("{}#L{}", self.blob_path(path), line_number)
    }
}

fn build_search_page(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
) -> Result<SearchPage> {
    let raw_query = api::get_query(url, "q").unwrap_or_default();
    let requested_scope = api::get_query(url, "scope").unwrap_or_else(|| "code".to_string());

    let parsed = api::parse_search_query(&raw_query);
    let effective_query = if parsed.fts_query.is_empty() {
        raw_query.clone()
    } else {
        parsed.fts_query.clone()
    };
    let scope = parsed
        .scope
        .map(|value| value.to_string())
        .unwrap_or_else(|| requested_scope.clone());
    let scope_inferred = scope != requested_scope;

    let (default_branch, _) = resolve_default_branch(sql)?;

    let state = if raw_query.is_empty() && effective_query.is_empty() {
        SearchPageState::Idle
    } else if scope == "commits" {
        SearchPageState::Commits {
            results: store::search_commits(sql, &effective_query, 50)?,
        }
    } else {
        let results = store::search_code(
            sql,
            &effective_query,
            parsed.path_filter.as_deref(),
            parsed.ext_filter.as_deref(),
            50,
        )?;
        let total_matches = results.iter().map(|result| result.matches.len()).sum();
        SearchPageState::Code {
            results,
            total_matches,
        }
    };

    Ok(SearchPage {
        route_path: url.path().to_string(),
        owner: owner.to_string(),
        repo_name: repo_name.to_string(),
        default_branch,
        raw_query,
        effective_query,
        requested_scope,
        scope,
        path_filter: parsed.path_filter,
        ext_filter: parsed.ext_filter,
        scope_inferred,
        state,
    })
}

fn render_search_html(page: &SearchPage, viewer: Viewer<'_>) -> String {
    let mut html = String::new();
    html.push_str("<h1>Search</h1>");

    let code_active = if page.scope == "code" {
        " style=\"font-weight:700;text-decoration:underline\""
    } else {
        ""
    };
    let commits_active = if page.scope == "commits" {
        " style=\"font-weight:700;text-decoration:underline\""
    } else {
        ""
    };
    html.push_str(&format!(
        r#"<div style="margin-bottom:12px;display:flex;gap:16px">
  <a href="{code_path}"{ca}>Code</a>
  <a href="{commits_path}"{cc}>Commits</a>
</div>"#,
        code_path = html_escape(&page.search_ui_path("code")),
        commits_path = html_escape(&page.search_ui_path("commits")),
        ca = code_active,
        cc = commits_active,
    ));

    html.push_str(&format!(
        r#"<form class="search-form" action="/{owner}/{repo}/search-ui" method="get">
  <input type="hidden" name="scope" value="{scope}">
  <input type="text" name="q" value="{q}" placeholder="Search... (@author: @message: @path: @ext: @content:)">
  <button type="submit">Search</button>
</form>"#,
        owner = page.owner,
        repo = page.repo_name,
        scope = html_escape(&page.scope),
        q = html_escape(&page.raw_query),
    ));

    match &page.state {
        SearchPageState::Idle => {}
        SearchPageState::Commits { results } => {
            if results.is_empty() {
                html.push_str("<p>No matching commits found.</p>");
            } else {
                html.push_str(&format!(
                    "<p>{} commit{} found</p>",
                    results.len(),
                    if results.len() == 1 { "" } else { "s" }
                ));
                html.push_str(r#"<ul class="commit-list">"#);
                for commit in results {
                    html.push_str(&format!(
                        r#"<li class="commit-item">
  <a class="commit-hash" href="/{owner}/{repo}/commit/{hash}">{short}</a>
  <span class="commit-msg"><a href="/{owner}/{repo}/commit/{hash}">{msg}</a></span>
  <span class="commit-author">{author}</span>
  <span class="commit-time">{time}</span>
</li>"#,
                        owner = page.owner,
                        repo = page.repo_name,
                        hash = commit.hash,
                        short = &commit.hash[..7.min(commit.hash.len())],
                        msg = html_escape(&first_line(&commit.message)),
                        author = html_escape(&commit.author),
                        time = format_time(commit.commit_time),
                    ));
                }
                html.push_str("</ul>");
            }
        }
        SearchPageState::Code {
            results,
            total_matches,
        } => {
            if results.is_empty() {
                html.push_str("<p>No results found.</p>");
            } else {
                html.push_str(&format!(
                    "<p>{} match{} across {} file{}</p>",
                    total_matches,
                    if *total_matches == 1 { "" } else { "es" },
                    results.len(),
                    if results.len() == 1 { "" } else { "s" },
                ));

                for result in results {
                    html.push_str(r#"<div class="search-result">"#);
                    html.push_str(&format!(
                        r#"<div class="search-result-path"><a href="/{owner}/{repo}/blob/{branch}/{path}">{path}</a> ({n} match{s})</div>"#,
                        owner = page.owner,
                        repo = page.repo_name,
                        branch = page.default_branch,
                        path = html_escape(&result.path),
                        n = result.matches.len(),
                        s = if result.matches.len() == 1 { "" } else { "es" },
                    ));

                    html.push_str(r#"<table class="diff-table" style="margin-top:4px">"#);
                    for item in &result.matches {
                        html.push_str(&format!(
                            r#"<tr class="diff-line-add"><td class="diff-ln"><a href="/{owner}/{repo}/blob/{branch}/{path}#L{ln}" style="color:#656d76">{ln}</a></td><td>{text}</td></tr>"#,
                            owner = page.owner,
                            repo = page.repo_name,
                            branch = page.default_branch,
                            path = html_escape(&result.path),
                            ln = item.line_number,
                            text = html_escape(&item.line_text),
                        ));
                    }
                    html.push_str("</table>");
                    html.push_str("</div>");
                }
            }
        }
    }

    layout(
        "Search",
        &page.owner,
        &page.repo_name,
        &page.default_branch,
        viewer,
        &html,
    )
}

fn render_search_markdown(page: &SearchPage, selection: &NegotiatedRepresentation) -> String {
    let mut markdown = format!("# {}/{} search\n", page.owner, page.repo_name);

    markdown.push_str(&format!(
        "\nQuery: {}\nScope: `{}`\n",
        markdown_value(&page.raw_query),
        page.scope
    ));

    if page.scope_inferred {
        markdown.push_str(&format!("Requested scope: `{}`\n", page.requested_scope));
    }
    if !page.effective_query.is_empty() && page.effective_query != page.raw_query {
        markdown.push_str(&format!(
            "Effective search terms: {}\n",
            markdown_value(&page.effective_query)
        ));
    }
    if let Some(path_filter) = &page.path_filter {
        markdown.push_str(&format!("Path filter: {}\n", markdown_value(path_filter)));
    }
    if let Some(ext_filter) = &page.ext_filter {
        markdown.push_str(&format!(
            "Extension filter: {}\n",
            markdown_value(ext_filter)
        ));
    }
    if page.scope == "code" {
        markdown.push_str(&format!("Indexed branch: `{}`\n", page.default_branch));
    }

    markdown.push_str("\n## Scope Navigation (GET paths)\n");
    markdown.push_str(&format!(
        "- `code`{} - `{}`\n",
        if page.scope == "code" {
            " (current)"
        } else {
            ""
        },
        page.search_ui_path("code")
    ));
    markdown.push_str(&format!(
        "- `commits`{} - `{}`\n",
        if page.scope == "commits" {
            " (current)"
        } else {
            ""
        },
        page.search_ui_path("commits")
    ));

    match &page.state {
        SearchPageState::Idle => {
            markdown.push_str("\nProvide `q=` to search code or commits.\n");
        }
        SearchPageState::Code {
            results,
            total_matches,
        } => {
            markdown.push_str(&format!(
                "\nResults: `{}` match{} across `{}` file{}\n",
                total_matches,
                if *total_matches == 1 { "" } else { "es" },
                results.len(),
                if results.len() == 1 { "" } else { "s" },
            ));

            markdown.push_str("\n## Code Results (GET paths)\n");
            if results.is_empty() {
                markdown.push_str("No code results found.\n");
            } else {
                for result in results {
                    markdown.push_str(&format!(
                        "- `{}` - {} match{} - `{}`\n",
                        result.path,
                        result.matches.len(),
                        if result.matches.len() == 1 { "" } else { "es" },
                        page.blob_path(&result.path)
                    ));
                    for item in &result.matches {
                        markdown.push_str(&format!(
                            "  - `L{}` - {} - `{}`\n",
                            item.line_number,
                            concise_line_text(&item.line_text),
                            page.blob_line_path(&result.path, item.line_number)
                        ));
                    }
                }
            }
        }
        SearchPageState::Commits { results } => {
            markdown.push_str(&format!(
                "\nResults: `{}` commit{}\n",
                results.len(),
                if results.len() == 1 { "" } else { "s" }
            ));

            markdown.push_str("\n## Commit Results (GET paths)\n");
            if results.is_empty() {
                markdown.push_str("No matching commits found.\n");
            } else {
                for commit in results {
                    markdown.push_str(&format!(
                        "- `{}` - {} - {} - {} - `{}`\n",
                        &commit.hash[..7.min(commit.hash.len())],
                        first_line(&commit.message),
                        commit.author,
                        format_time(commit.commit_time),
                        page.commit_path(&commit.hash)
                    ));
                }
            }
        }
    }

    let actions = vec![
        Action::get(page.current_path(), "reload this search page"),
        Action::get(page.search_ui_path("code"), "switch to code search"),
        Action::get(page.search_ui_path("commits"), "switch to commit search"),
        Action::get(
            page.json_search_path(),
            "fetch the structured JSON search endpoint",
        ),
    ];

    let mut hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new("Use `/search-ui` for the human-readable page and `/search` for structured JSON results."),
        Hint::new("Supported filters: `@author:`, `@message:`, `@path:`, `@ext:`, and `@content:`."),
        Hint::new("`@author:` and `@message:` imply commit scope; `@path:` and `@ext:` narrow code search results."),
    ];
    if page.scope == "code" {
        hints.push(Hint::new(format!(
            "Code results come from the default branch index: `{}`.",
            page.default_branch
        )));
    }

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    markdown
}

fn build_query_path(base_path: &str, params: &[(&str, Option<&str>)]) -> String {
    let mut url = match Url::parse("https://ripgit.local/") {
        Ok(url) => url,
        Err(_) => return base_path.to_string(),
    };
    url.set_path(base_path);

    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            if let Some(value) = value {
                query.append_pair(key, value);
            }
        }
    }

    match url.query() {
        Some(query) if !query.is_empty() => format!("{}?{}", url.path(), query),
        _ => url.path().to_string(),
    }
}

fn markdown_value(value: &str) -> String {
    if value.is_empty() {
        "(empty)".to_string()
    } else {
        format!("`{}`", value)
    }
}

fn concise_line_text(text: &str) -> String {
    const MAX_CHARS: usize = 160;
    if text.chars().count() <= MAX_CHARS {
        return text.to_string();
    }

    let mut output = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= MAX_CHARS - 3 {
            break;
        }
        output.push(ch);
    }
    output.push_str("...");
    output
}

pub fn page_search(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let page = build_search_page(sql, owner, repo_name, url)?;
    html_response(&render_search_html(&page, viewer))
}

pub fn page_search_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let page = build_search_page(sql, owner, repo_name, url)?;
    presentation::markdown_response(&render_search_markdown(&page, selection), selection)
}
