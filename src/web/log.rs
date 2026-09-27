use super::*;

struct LogPage {
    route_path: String,
    owner: String,
    repo_name: String,
    ref_name: String,
    current_page: i64,
    per_page: i64,
    branches: Vec<String>,
    commits: Vec<CommitListItem>,
    has_next: bool,
}

impl LogPage {
    fn previous_page(&self) -> Option<i64> {
        (self.current_page > 1).then_some(self.current_page - 1)
    }

    fn next_page(&self) -> Option<i64> {
        self.has_next.then_some(self.current_page + 1)
    }

    fn log_path(&self, ref_name: &str, page: i64) -> String {
        let mut path = format!("{}?ref={}", self.route_path, ref_name);
        if page > 1 {
            path.push_str(&format!("&page={}", page));
        }
        path
    }

    fn current_path(&self) -> String {
        self.log_path(&self.ref_name, self.current_page)
    }

    fn commit_path(&self, hash: &str) -> String {
        format!("/{}/{}/commit/{}", self.owner, self.repo_name, hash)
    }

    fn home_path(&self) -> String {
        format!("/{}/{}/?ref={}", self.owner, self.repo_name, self.ref_name)
    }

    fn search_path(&self) -> String {
        format!("/{}/{}/search-ui?scope=commits", self.owner, self.repo_name)
    }

    fn json_log_path(&self) -> String {
        let offset = (self.current_page - 1) * self.per_page;
        format!(
            "/{}/{}/log?ref={}&limit={}&offset={}",
            self.owner, self.repo_name, self.ref_name, self.per_page, offset
        )
    }
}

fn build_log_page(sql: &SqlStorage, owner: &str, repo_name: &str, url: &Url) -> Result<LogPage> {
    let ref_name = api::get_query(url, "ref").unwrap_or_else(|| {
        resolve_default_branch(sql)
            .map(|(name, _)| name)
            .unwrap_or_else(|_| "main".to_string())
    });
    let current_page: i64 = api::get_query(url, "page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    let per_page: i64 = 30;
    let offset = (current_page - 1) * per_page;
    let branches = load_branches(sql)?;

    let (commits, has_next) = match api::resolve_ref(sql, &ref_name)? {
        Some(head) => {
            let commits = walk_commits(sql, &head, per_page + 1, offset)?;
            let has_next = commits.len() as i64 > per_page;
            let commits = summarize_commits(commits.into_iter().take(per_page as usize).collect());
            (commits, has_next)
        }
        None => (Vec::new(), false),
    };

    Ok(LogPage {
        route_path: url.path().to_string(),
        owner: owner.to_string(),
        repo_name: repo_name.to_string(),
        ref_name,
        current_page,
        per_page,
        branches,
        commits,
        has_next,
    })
}

fn render_log_branch_selector(page: &LogPage) -> String {
    if page.branches.len() <= 1 {
        return format!(
            r#"<div class="branch-selector"><span class="branch-label">branch:</span> <strong>{}</strong></div>"#,
            html_escape(&page.ref_name)
        );
    }

    let mut html = String::from(
        r#"<div class="branch-selector"><span class="branch-label">branch:</span> <select onchange="window.location.href=this.value">"#,
    );

    for branch in &page.branches {
        let selected = if branch == &page.ref_name {
            " selected"
        } else {
            ""
        };
        html.push_str(&format!(
            r#"<option value="{}"{}>{}</option>"#,
            html_escape(&page.log_path(branch, 1)),
            selected,
            html_escape(branch)
        ));
    }

    html.push_str("</select></div>");
    html
}

fn render_log_html(page: &LogPage, viewer: Viewer<'_>) -> String {
    let mut html = String::new();
    html.push_str(&render_log_branch_selector(page));
    html.push_str(&format!(
        r#"<h1>Commits on <strong>{}</strong></h1>"#,
        html_escape(&page.ref_name)
    ));

    if page.commits.is_empty() {
        html.push_str("<p>No commits yet.</p>");
    } else {
        html.push_str(&render_commit_list(
            &page.commits,
            &page.owner,
            &page.repo_name,
            true,
        ));
    }

    html.push_str(r#"<div class="pagination">"#);
    if let Some(previous_page) = page.previous_page() {
        html.push_str(&format!(
            r#"<a href="{}">Previous</a>"#,
            html_escape(&page.log_path(&page.ref_name, previous_page))
        ));
    }
    if let Some(next_page) = page.next_page() {
        html.push_str(&format!(
            r#"<a href="{}">Next</a>"#,
            html_escape(&page.log_path(&page.ref_name, next_page))
        ));
    }
    html.push_str("</div>");

    layout(
        "Commits",
        &page.owner,
        &page.repo_name,
        &page.ref_name,
        viewer,
        &html,
    )
}

fn render_log_markdown(page: &LogPage, selection: &NegotiatedRepresentation) -> String {
    let mut markdown = format!(
        "# {}/{} commits\n\nBranch: `{}`\nPage: `{}`\n",
        page.owner, page.repo_name, page.ref_name, page.current_page
    );

    if page.commits.is_empty() {
        markdown.push_str("\nNo commits yet.\n");
    } else {
        markdown.push_str("\n## Commits (GET paths)\n");
        for commit in &page.commits {
            markdown.push_str(&format!(
                "- `{}` - {} - {} - {} - `{}`\n",
                commit.short_hash,
                commit.subject,
                commit.author,
                commit.relative_time,
                page.commit_path(&commit.hash)
            ));
        }
    }

    if !page.branches.is_empty() {
        markdown.push_str("\n## Branches (GET paths)\n");
        for branch in &page.branches {
            let current = if branch == &page.ref_name {
                " (current)"
            } else {
                ""
            };
            markdown.push_str(&format!(
                "- `{}`{} - `{}`\n",
                branch,
                current,
                page.log_path(branch, 1)
            ));
        }
    }

    let mut actions = vec![
        Action::get(page.current_path(), "reload this commit log page"),
        Action::get(page.home_path(), "browse the repository root at this ref"),
        Action::get(page.search_path(), "search commit messages and authors"),
        Action::get(page.json_log_path(), "fetch this page as structured JSON"),
    ];

    if let Some(previous_page) = page.previous_page() {
        actions.push(Action::get(
            page.log_path(&page.ref_name, previous_page),
            "view the previous page of commits",
        ));
    }
    if let Some(next_page) = page.next_page() {
        actions.push(Action::get(
            page.log_path(&page.ref_name, next_page),
            "view the next page of commits",
        ));
    }

    let hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new("Pagination uses `page=` here and `offset=` plus `limit=` on the JSON API."),
        Hint::new("The `/log` and `/commits` routes now render the same commit-log page model."),
    ];

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    markdown
}

pub fn page_log(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let page = build_log_page(sql, owner, repo_name, url)?;
    html_response(&render_log_html(&page, viewer))
}

pub fn page_log_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let page = build_log_page(sql, owner, repo_name, url)?;
    presentation::markdown_response(&render_log_markdown(&page, selection), selection)
}
