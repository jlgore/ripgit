use crate::web::Viewer;
use crate::{api, issues, presentation, web};
use worker::*;

use super::Url;
use presentation::{Action, Hint, NegotiatedRepresentation};

const PAGE_SIZE: usize = 25;

struct ListPage {
    owner: String,
    repo_name: String,
    kind: String,
    kind_label: &'static str,
    kind_url: &'static str,
    new_label: &'static str,
    new_url: &'static str,
    default_branch: String,
    state: String,
    offset: usize,
    items: Vec<issues::IssueRow>,
    has_more: bool,
    open_count: i64,
    closed_count: i64,
}

impl ListPage {
    fn base_path(&self) -> String {
        format!("/{}/{}/{}", self.owner, self.repo_name, self.kind_url)
    }

    fn new_path(&self) -> String {
        format!("/{}/{}/{}", self.owner, self.repo_name, self.new_url)
    }

    fn item_path(&self, number: i64) -> String {
        format!(
            "/{}/{}/{}/{}",
            self.owner, self.repo_name, self.kind_url, number
        )
    }

    fn list_path(&self, state: &str, offset: usize) -> String {
        let mut path = self.base_path();
        let mut query = Vec::new();

        if state != "open" {
            query.push(format!("state={}", state));
        }
        if offset > 0 {
            query.push(format!("offset={}", offset));
        }

        if !query.is_empty() {
            path.push('?');
            path.push_str(&query.join("&"));
        }

        path
    }

    fn current_path(&self) -> String {
        self.list_path(&self.state, self.offset)
    }

    fn open_path(&self) -> String {
        self.list_path("open", 0)
    }

    fn closed_path(&self) -> String {
        self.list_path("closed", 0)
    }

    fn previous_path(&self) -> Option<String> {
        (self.offset > 0)
            .then(|| self.list_path(&self.state, self.offset.saturating_sub(PAGE_SIZE)))
    }

    fn next_path(&self) -> Option<String> {
        self.has_more
            .then(|| self.list_path(&self.state, self.offset + PAGE_SIZE))
    }

    fn item_label(&self) -> &'static str {
        if self.kind == "pr" {
            "Pull request"
        } else {
            "Issue"
        }
    }
}

fn build_list_page(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    kind: &str,
) -> Result<ListPage> {
    let (default_branch, _) = web::resolve_default_branch(sql)?;
    let state = api::get_query(url, "state").unwrap_or_else(|| "open".to_string());
    let offset: usize = api::get_query(url, "offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let items = issues::list_issues(sql, kind, &state, PAGE_SIZE + 1, offset)?;
    let has_more = items.len() > PAGE_SIZE;
    let items = items.into_iter().take(PAGE_SIZE).collect();

    let open_count = issues::count_issues(sql, kind, "open")?;
    let closed_count = issues::count_issues_not_open(sql, kind)?;

    let (kind_label, kind_url, new_label, new_url) = if kind == "pr" {
        ("Pull Requests", "pulls", "New pull request", "pulls/new")
    } else {
        ("Issues", "issues", "New issue", "issues/new")
    };

    Ok(ListPage {
        owner: owner.to_string(),
        repo_name: repo_name.to_string(),
        kind: kind.to_string(),
        kind_label,
        kind_url,
        new_label,
        new_url,
        default_branch,
        state,
        offset,
        items,
        has_more,
        open_count,
        closed_count,
    })
}

pub fn page_issues_list(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let page = build_list_page(sql, owner, repo_name, url, "issue")?;
    render_list_html(&page, viewer)
}

pub fn page_pulls_list(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let page = build_list_page(sql, owner, repo_name, url, "pr")?;
    render_list_html(&page, viewer)
}

pub fn page_issues_list_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    viewer: Viewer<'_>,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let page = build_list_page(sql, owner, repo_name, url, "issue")?;
    presentation::markdown_response(
        &render_list_markdown(&page, viewer, selection),
        selection,
    )
}

pub fn page_pulls_list_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    viewer: Viewer<'_>,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let page = build_list_page(sql, owner, repo_name, url, "pr")?;
    presentation::markdown_response(
        &render_list_markdown(&page, viewer, selection),
        selection,
    )
}

fn render_list_html(page: &ListPage, viewer: Viewer<'_>) -> Result<Response> {
    let is_open_tab = page.state == "open";

    let tabs = format!(
        r#"<div class="issue-tabs">
          <a href="{open_href}" class="issue-tab{open_active}">{open_count} Open</a>
          <a href="{closed_href}" class="issue-tab{closed_active}">{closed_count} Closed</a>
        </div>"#,
        open_href = web::html_escape(&page.open_path()),
        open_active = if is_open_tab { " active" } else { "" },
        open_count = page.open_count,
        closed_href = web::html_escape(&page.closed_path()),
        closed_active = if !is_open_tab { " active" } else { "" },
        closed_count = page.closed_count,
    );

    let new_btn = if viewer.name.is_some() {
        format!(
            r#"<a href="{}" class="btn-primary">{}</a>"#,
            web::html_escape(&page.new_path()),
            page.new_label
        )
    } else {
        String::new()
    };

    let mut items_html = String::new();
    if page.items.is_empty() {
        items_html.push_str(r#"<div class="issue-empty">No items found.</div>"#);
    } else {
        for item in &page.items {
            let state_class = match item.state.as_str() {
                "merged" => "merged",
                "closed" => "closed",
                _ => "open",
            };
            let state_icon = match item.state.as_str() {
                "merged" => "⟳",
                "closed" => "✓",
                _ => "●",
            };
            let branch_info = if page.kind == "pr" {
                match (&item.source_branch, &item.target_branch) {
                    (Some(src), Some(tgt)) => format!(
                        r#" <span class="pr-branch-pair"><code>{}</code> → <code>{}</code></span>"#,
                        web::html_escape(src),
                        web::html_escape(tgt)
                    ),
                    _ => String::new(),
                }
            } else {
                String::new()
            };
            items_html.push_str(&format!(
                r#"<div class="issue-item">
                  <span class="issue-state-icon {state_class}">{icon}</span>
                  <div class="issue-item-main">
                    <a href="{href}" class="issue-item-title">{title}</a>{branch_info}
                    <div class="issue-item-meta">
                      #{num} opened {time} by <strong>{author}</strong>
                    </div>
                  </div>
                </div>"#,
                state_class = state_class,
                icon = state_icon,
                href = web::html_escape(&page.item_path(item.number)),
                title = web::html_escape(&item.title),
                branch_info = branch_info,
                num = item.number,
                time = web::format_time(item.created_at),
                author = web::html_escape(&item.author_name),
            ));
        }
    }

    let mut pagination = String::new();
    if let Some(previous_path) = page.previous_path() {
        pagination.push_str(&format!(
            r#"<a href="{}" class="btn-action">← Newer</a>"#,
            web::html_escape(&previous_path)
        ));
    }
    if let Some(next_path) = page.next_path() {
        pagination.push_str(&format!(
            r#"<a href="{}" class="btn-action">Older →</a>"#,
            web::html_escape(&next_path)
        ));
    }
    if !pagination.is_empty() {
        pagination = format!(r#"<div class="pagination">{}</div>"#, pagination);
    }

    let content = format!(
        r#"<div class="issue-list-header">
          <h1>{kind_label}</h1>
          {new_btn}
        </div>
        {tabs}
        <div class="issue-list">{items_html}</div>
        {pagination}"#,
        kind_label = page.kind_label,
        new_btn = new_btn,
        tabs = tabs,
        items_html = items_html,
        pagination = pagination,
    );

    web::html_response(&web::layout(
        page.kind_label,
        &page.owner,
        &page.repo_name,
        &page.default_branch,
        viewer,
        &content,
    ))
}

fn render_list_markdown(
    page: &ListPage,
    viewer: Viewer<'_>,
    selection: &NegotiatedRepresentation,
) -> String {
    let mut markdown = format!(
        "# {}/{} {}\n\nCurrent state filter: `{}`\nCounts: `{}` open, `{}` not open\nPagination: offset=`{}`, page_size=`{}`\n",
        page.owner,
        page.repo_name,
        page.kind_label,
        page.state,
        page.open_count,
        page.closed_count,
        page.offset,
        PAGE_SIZE,
    );

    match (page.previous_path(), page.next_path()) {
        (Some(previous_path), Some(next_path)) => markdown.push_str(&format!(
            "Previous page: `{}`\nNext page: `{}`\n",
            previous_path, next_path
        )),
        (Some(previous_path), None) => {
            markdown.push_str(&format!(
                "Previous page: `{}`\nNext page: none\n",
                previous_path
            ));
        }
        (None, Some(next_path)) => {
            markdown.push_str(&format!(
                "Previous page: none\nNext page: `{}`\n",
                next_path
            ));
        }
        (None, None) => markdown.push_str("Previous page: none\nNext page: none\n"),
    }

    markdown.push_str("\n## Items\n");
    if page.items.is_empty() {
        markdown.push_str("No items found for this state filter.\n");
    } else {
        for item in &page.items {
            let mut line = format!(
                "- #{} - {} - {} - opened {} by {}",
                item.number,
                item.state,
                item.title,
                web::format_time(item.created_at),
                item.author_name,
            );
            if page.kind == "pr" {
                if let (Some(source_branch), Some(target_branch)) =
                    (&item.source_branch, &item.target_branch)
                {
                    line.push_str(&format!(
                        " - branches: `{}` -> `{}`",
                        source_branch, target_branch
                    ));
                }
            }
            markdown.push_str(&line);
            markdown.push('\n');
        }
    }

    markdown.push_str("\n## Item Paths (GET paths)\n");
    if page.items.is_empty() {
        markdown.push_str("No item paths on this page.\n");
    } else {
        for item in &page.items {
            markdown.push_str(&format!("- `{}`\n", page.item_path(item.number)));
        }
    }

    let mut actions = vec![
        Action::get(page.current_path(), "reload this list page"),
        Action::get(page.open_path(), "view open items"),
        Action::get(page.closed_path(), "view closed items"),
    ];

    if viewer.name.is_some() {
        actions.push(Action::get(
            page.new_path(),
            format!("open the {} form", page.new_label),
        ));
    } else {
        actions.push(
            Action::get(page.new_path(), format!("open the {} form", page.new_label))
                .with_requires("authenticated user"),
        );
    }

    if let Some(previous_path) = page.previous_path() {
        actions.push(Action::get(
            previous_path,
            "view the previous page of results",
        ));
    }
    if let Some(next_path) = page.next_path() {
        actions.push(Action::get(next_path, "view the next page of results"));
    }

    let closed_hint = if page.kind == "pr" {
        "`closed` and `merged` pull requests both count as not open in the list summary."
    } else {
        "The closed count is reported with the existing not-open summary query."
    };

    let hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new(format!(
            "Item detail pages live under `/{}/{}/{}/{{number}}`.",
            page.owner, page.repo_name, page.kind_url
        )),
        Hint::new(closed_hint),
        Hint::new(format!(
            "{} items are listed newest first, with {} results per page.",
            page.item_label(),
            PAGE_SIZE
        )),
    ];

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    markdown
}
