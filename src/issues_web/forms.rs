use crate::web::Viewer;
use crate::{api, presentation, web};
use worker::*;

use super::{load_branches, Url};
use presentation::{Action, ActionField, Hint, NegotiatedRepresentation};

struct IssueFormPage {
    owner: String,
    repo_name: String,
    default_branch: String,
}

impl IssueFormPage {
    fn new(sql: &SqlStorage, owner: &str, repo_name: &str) -> Result<Self> {
        let (default_branch, _) = web::resolve_default_branch(sql)?;
        Ok(Self {
            owner: owner.to_string(),
            repo_name: repo_name.to_string(),
            default_branch,
        })
    }

    fn form_path(&self) -> String {
        format!("/{}/{}/issues/new", self.owner, self.repo_name)
    }

    fn submit_path(&self) -> String {
        format!("/{}/{}/issues", self.owner, self.repo_name)
    }
}

struct PullFormPage {
    owner: String,
    repo_name: String,
    default_branch: String,
    branches: Vec<String>,
    preselect_source: String,
    preselect_target: String,
}

impl PullFormPage {
    fn new(sql: &SqlStorage, owner: &str, repo_name: &str, url: &Url) -> Result<Self> {
        let (default_branch, _) = web::resolve_default_branch(sql)?;
        let branches = load_branches(sql)?;
        let preselect_source = api::get_query(url, "source").unwrap_or_default();
        let preselect_target =
            api::get_query(url, "target").unwrap_or_else(|| default_branch.clone());

        Ok(Self {
            owner: owner.to_string(),
            repo_name: repo_name.to_string(),
            default_branch,
            branches,
            preselect_source,
            preselect_target,
        })
    }

    fn form_path(&self) -> String {
        format!("/{}/{}/pulls/new", self.owner, self.repo_name)
    }

    fn submit_path(&self) -> String {
        format!("/{}/{}/pulls", self.owner, self.repo_name)
    }

    fn has_enough_branches(&self) -> bool {
        self.branches.len() >= 2
    }
}

pub fn page_new_issue(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let page = IssueFormPage::new(sql, owner, repo_name)?;

    let content = format!(
        r#"<h1>New Issue</h1>
        <form method="POST" action="/{owner}/{repo}/issues" class="new-issue-form">
          <div class="form-group">
            <label class="form-label" for="title">Title</label>
            <input type="text" id="title" name="title" class="form-input"
                   placeholder="Issue title" required autofocus>
          </div>
          <div class="form-group">
            <label class="form-label" for="body">Description <span class="form-hint">(Markdown)</span></label>
            <textarea id="body" name="body" class="form-textarea"
                      placeholder="Describe the issue..." rows="12"></textarea>
          </div>
          <div class="form-actions">
            <button type="submit" class="btn-primary">Submit new issue</button>
            <a href="/{owner}/{repo}/issues" class="btn-action">Cancel</a>
          </div>
        </form>"#,
        owner = web::html_escape(&page.owner),
        repo = web::html_escape(&page.repo_name),
    );

    web::html_response(&web::layout(
        "New Issue",
        &page.owner,
        &page.repo_name,
        &page.default_branch,
        viewer,
        &content,
    ))
}

pub fn page_new_issue_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    _viewer: Viewer<'_>,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let page = IssueFormPage::new(sql, owner, repo_name)?;
    presentation::markdown_response(&render_new_issue_markdown(&page, selection), selection)
}

pub fn page_new_pull(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let page = PullFormPage::new(sql, owner, repo_name, url)?;

    if !page.has_enough_branches() {
        let content = r#"<p>You need at least two branches to open a pull request.</p>"#;
        return web::html_response(&web::layout(
            "New Pull Request",
            &page.owner,
            &page.repo_name,
            &page.default_branch,
            viewer,
            content,
        ));
    }

    let source_options: String = page
        .branches
        .iter()
        .map(|branch| {
            let selected = if branch == &page.preselect_source {
                " selected"
            } else {
                ""
            };
            format!(
                r#"<option value="{value}"{selected}>{value}</option>"#,
                value = web::html_escape(branch),
                selected = selected
            )
        })
        .collect();

    let target_options: String = page
        .branches
        .iter()
        .map(|branch| {
            let selected = if branch == &page.preselect_target {
                " selected"
            } else {
                ""
            };
            format!(
                r#"<option value="{value}"{selected}>{value}</option>"#,
                value = web::html_escape(branch),
                selected = selected
            )
        })
        .collect();

    let content = format!(
        r#"<h1>New Pull Request</h1>
        <form method="POST" action="/{owner}/{repo}/pulls" class="new-issue-form">
          <div class="form-group form-branch-row">
            <div>
              <label class="form-label">Base branch <span class="form-hint">(merge into)</span></label>
              <select name="target" class="branch-input">{target_options}</select>
            </div>
            <span class="arrow">←</span>
            <div>
              <label class="form-label">Compare branch <span class="form-hint">(changes from)</span></label>
              <select name="source" class="branch-input">{source_options}</select>
            </div>
          </div>
          <div class="form-group">
            <label class="form-label" for="pr-title">Title</label>
            <input type="text" id="pr-title" name="title" class="form-input"
                   placeholder="Pull request title" required autofocus>
          </div>
          <div class="form-group">
            <label class="form-label" for="pr-body">Description <span class="form-hint">(Markdown)</span></label>
            <textarea id="pr-body" name="body" class="form-textarea"
                      placeholder="Describe your changes..." rows="10"></textarea>
          </div>
          <div class="form-actions">
            <button type="submit" class="btn-primary">Open pull request</button>
            <a href="/{owner}/{repo}/pulls" class="btn-action">Cancel</a>
          </div>
        </form>"#,
        owner = web::html_escape(&page.owner),
        repo = web::html_escape(&page.repo_name),
        source_options = source_options,
        target_options = target_options,
    );

    web::html_response(&web::layout(
        "New Pull Request",
        &page.owner,
        &page.repo_name,
        &page.default_branch,
        viewer,
        &content,
    ))
}

pub fn page_new_pull_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    _viewer: Viewer<'_>,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let page = PullFormPage::new(sql, owner, repo_name, url)?;
    presentation::markdown_response(&render_new_pull_markdown(&page, selection), selection)
}

fn render_new_issue_markdown(page: &IssueFormPage, selection: &NegotiatedRepresentation) -> String {
    let mut markdown = format!(
        "# New issue - {}/{}\n\nPOST endpoint: `{}`\nAuthentication: required\nRequired fields: `title`\nOptional fields: `body`\nCancel path: `{}`\nList path: `{}`\nResult: creates a new open issue.\n",
        page.owner,
        page.repo_name,
        page.submit_path(),
        page.submit_path(),
        page.submit_path(),
    );

    markdown.push_str("\n## Related Paths (GET paths)\n");
    markdown.push_str(&format!("- `{}`\n", page.form_path()));
    markdown.push_str(&format!("- `{}`\n", page.submit_path()));

    let actions = vec![
        Action::post(page.submit_path(), "create a new issue")
            .with_fields(vec![
                ActionField::required("title", "short issue summary; must be non-empty"),
                ActionField::optional("body", "markdown description for the issue body"),
            ])
            .with_requires("authenticated user")
            .with_effect("stores an open issue and redirects to its detail page"),
        Action::get(page.submit_path(), "list existing issues"),
        Action::get(page.form_path(), "reload this form description"),
    ];

    let hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new("The HTML form marks `title` as required and autofocuses it."),
        Hint::new("`body` accepts markdown and may be left empty."),
        Hint::new("Use the issues list path as the cancel destination for this form."),
    ];

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    markdown
}

fn render_new_pull_markdown(page: &PullFormPage, selection: &NegotiatedRepresentation) -> String {
    let mut markdown = format!(
        "# New pull request - {}/{}\n\nPOST endpoint: `{}`\nAuthentication: required\nRequired fields: `target`, `source`, `title`\nOptional fields: `body`\nConstraint: `source` and `target` must differ.\nCancel path: `{}`\nList path: `{}`\nDefault target branch: `{}`\nPreselected source branch: `{}`\nPreselected target branch: `{}`\n",
        page.owner,
        page.repo_name,
        page.submit_path(),
        page.submit_path(),
        page.submit_path(),
        page.default_branch,
        if page.preselect_source.is_empty() {
            "none"
        } else {
            &page.preselect_source
        },
        if page.preselect_target.is_empty() {
            "none"
        } else {
            &page.preselect_target
        },
    );

    if page.has_enough_branches() {
        markdown.push_str("Pull request creation is available from this form.\n");
    } else {
        markdown.push_str("Pull request creation is currently unavailable because fewer than two branches exist.\n");
    }

    markdown.push_str("\n## Branches\n");
    if page.branches.is_empty() {
        markdown.push_str("No branches found.\n");
    } else {
        for branch in &page.branches {
            let mut suffix = String::new();
            if branch == &page.default_branch {
                suffix.push_str(" (default target)");
            }
            if branch == &page.preselect_source {
                suffix.push_str(" (preselected source)");
            }
            if branch == &page.preselect_target {
                suffix.push_str(" (preselected target)");
            }
            markdown.push_str(&format!("- `{}`{}\n", branch, suffix));
        }
    }

    markdown.push_str("\n## Related Paths (GET paths)\n");
    markdown.push_str(&format!("- `{}`\n", page.form_path()));
    markdown.push_str(&format!("- `{}`\n", page.submit_path()));

    let mut actions = vec![
        Action::get(page.submit_path(), "list existing pull requests"),
        Action::get(page.form_path(), "reload this form description"),
    ];

    let post_action = Action::post(page.submit_path(), "create a new pull request")
        .with_fields(vec![
            ActionField::required("target", "base branch to merge into"),
            ActionField::required("source", "compare branch containing the proposed changes"),
            ActionField::required("title", "pull request summary; must be non-empty"),
            ActionField::optional("body", "markdown description for the pull request body"),
        ])
        .with_requires("authenticated user")
        .with_effect("stores an open pull request and redirects to its detail page");

    if page.has_enough_branches() {
        actions.insert(0, post_action);
    } else {
        actions.insert(
            0,
            post_action.with_effect(
                "requires at least two existing branches before the HTML form can be used",
            ),
        );
    }

    let hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new("`source` and `target` are both required, must name existing branches, and must differ."),
        Hint::new("The HTML form defaults `target` to the repository default branch when no query override is provided."),
        Hint::new("Use `?source=<branch>&target=<branch>` on the new-pull path to preselect branch choices."),
        Hint::new("Use the pulls list path as the cancel destination for this form."),
    ];

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    markdown
}
