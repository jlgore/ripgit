use crate::authz::Role;
use crate::web::Viewer;
use crate::{api, diff, issues, presentation, web};
use worker::*;

enum PrDiffData {
    SourceMissing {
        source_branch: String,
    },
    TargetMissing {
        target_branch: String,
    },
    Compared {
        files: Vec<diff::FileDiff>,
        stats: diff::DiffStats,
    },
}

pub fn page_issue_detail(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    number: i64,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let (default_branch, _) = web::resolve_default_branch(sql)?;

    let issue = match issues::get_issue(sql, number)? {
        Some(issue) => issue,
        None => return Response::error("Not Found", 404),
    };

    let comments = issues::list_comments(sql, issue.id)?;

    let is_pr = issue.kind == "pr";
    let kind_url = if is_pr { "pulls" } else { "issues" };

    let state_class = match issue.state.as_str() {
        "merged" => "merged",
        "closed" => "closed",
        _ => "open",
    };
    let state_label = match issue.state.as_str() {
        "merged" => "Merged",
        "closed" => "Closed",
        _ => "Open",
    };

    let pr_diff_section = if is_pr {
        render_pr_diff_section(sql, owner, repo_name, &issue, viewer)?
    } else {
        String::new()
    };

    let mut comments_html = String::new();
    for comment in &comments {
        comments_html.push_str(&render_comment(
            &comment.author_name,
            comment.created_at,
            &comment.body,
            false,
            "",
        ));
    }

    let comment_form = if viewer.name.is_some() {
        format!(
            r#"<div class="comment comment-form-wrap">
              <div class="comment-header-row">
                <strong>{actor}</strong>
              </div>
              <form method="POST" action="/{owner}/{repo}/{kind_url}/{num}/comment" class="comment-form">
                <textarea name="body" class="comment-textarea" placeholder="Leave a comment..." rows="5"></textarea>
                <div class="comment-form-footer">
                  {state_btn}
                  <button type="submit" class="btn-primary">Comment</button>
                </div>
              </form>
            </div>"#,
            actor = web::html_escape(viewer.name.unwrap_or("")),
            owner = web::html_escape(owner),
            repo = web::html_escape(repo_name),
            kind_url = kind_url,
            num = number,
            state_btn = render_state_button(&issue, owner, repo_name, kind_url, viewer),
        )
    } else {
        String::new()
    };

    let branch_meta = if is_pr {
        match (&issue.source_branch, &issue.target_branch) {
            (Some(src), Some(tgt)) => format!(
                r#"<div class="pr-branch-info">
                  <code class="branch-tag">{}</code>
                  <span class="arrow">→</span>
                  <code class="branch-tag">{}</code>
                </div>"#,
                web::html_escape(src),
                web::html_escape(tgt),
            ),
            _ => String::new(),
        }
    } else {
        String::new()
    };

    let page_title = if is_pr { "Pull Request" } else { "Issue" };

    let content = format!(
        r#"<div class="issue-detail-header">
          <div class="issue-title-row">
            <h1>{title} <span class="issue-number-heading">#{num}</span></h1>
          </div>
          <div class="issue-meta-row">
            <span class="issue-badge {state_class}">{state_label}</span>
            {branch_meta}
            <span class="issue-meta-text">
              opened {time} by <strong>{author}</strong>
            </span>
          </div>
        </div>

        <div class="issue-body-wrap">
          {body_html}
        </div>

        {pr_diff_section}

        <div class="comment-thread">
          {comments_html}
        </div>

        {comment_form}"#,
        title = web::html_escape(&issue.title),
        num = number,
        state_class = state_class,
        state_label = state_label,
        branch_meta = branch_meta,
        time = web::format_time(issue.created_at),
        author = web::html_escape(&issue.author_name),
        body_html = render_issue_body(&issue.body),
        pr_diff_section = pr_diff_section,
        comments_html = comments_html,
        comment_form = comment_form,
    );

    web::html_response(&web::layout(
        &format!("{} #{}", page_title, number),
        owner,
        repo_name,
        &default_branch,
        viewer,
        &content,
    ))
}

pub fn page_issue_detail_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    number: i64,
    viewer: Viewer<'_>,
    selection: &presentation::NegotiatedRepresentation,
) -> Result<Response> {
    let issue = match issues::get_issue(sql, number)? {
        Some(issue) => issue,
        None => return Response::error("Not Found", 404),
    };

    let comments = issues::list_comments(sql, issue.id)?;
    let markdown = render_issue_detail_markdown(
        sql, owner, repo_name, &issue, &comments, viewer, selection,
    )?;
    presentation::markdown_response(&markdown, selection)
}

fn render_issue_detail_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    issue: &issues::IssueRow,
    comments: &[issues::CommentRow],
    viewer: Viewer<'_>,
    selection: &presentation::NegotiatedRepresentation,
) -> Result<String> {
    let is_pr = issue.kind == "pr";
    let kind_label = if is_pr { "Pull Request" } else { "Issue" };
    let kind_url = if is_pr { "pulls" } else { "issues" };
    let detail_path = format!("/{}/{}/{}/{}", owner, repo_name, kind_url, issue.number);
    let list_path = format!("/{}/{}/{}", owner, repo_name, kind_url);

    let mut markdown = format!(
        "# {} #{}: {}\n\n- State: `{}`\n- Author: `{}`\n- Created: {}\n- Updated: {}\n- Comments: `{}`\n- Path: `{}`\n",
        kind_label,
        issue.number,
        issue.title,
        issue_state_label(&issue.state),
        issue.author_name,
        web::format_time(issue.created_at),
        web::format_time(issue.updated_at),
        comments.len(),
        detail_path,
    );

    if is_pr {
        markdown.push_str(&render_pr_markdown_section(
            sql, owner, repo_name, issue, viewer,
        )?);
    }

    markdown.push_str("\n## Description\n");
    if issue.body.trim().is_empty() {
        markdown.push_str("No description provided.\n");
    } else {
        markdown.push('\n');
        markdown.push_str(&markdown_literal_block(&issue.body));
    }

    markdown.push_str("\n## Comments\n");
    if comments.is_empty() {
        markdown.push_str("No comments yet.\n");
    } else {
        for (idx, comment) in comments.iter().enumerate() {
            markdown.push_str(&format!(
                "\n### Comment {} - {} - {}\n\n",
                idx + 1,
                comment.author_name,
                web::format_time(comment.created_at),
            ));
            if comment.body.trim().is_empty() {
                markdown.push_str("(empty comment)\n");
            } else {
                markdown.push_str(&markdown_literal_block(&comment.body));
            }
        }
    }

    let mut related_paths = vec![
        ("detail", detail_path.clone()),
        (if is_pr { "pull requests" } else { "issues" }, list_path),
    ];
    if is_pr {
        if let Some(source_branch) = &issue.source_branch {
            related_paths.push((
                "source branch",
                format!("/{}/{}/?ref={}", owner, repo_name, source_branch),
            ));
        }
        if let Some(target_branch) = &issue.target_branch {
            related_paths.push((
                "target branch",
                format!("/{}/{}/?ref={}", owner, repo_name, target_branch),
            ));
        }
        if let Some(merge_hash) = &issue.merge_commit_hash {
            related_paths.push((
                "merge commit",
                format!("/{}/{}/commit/{}", owner, repo_name, merge_hash),
            ));
        }
    }
    markdown.push_str(&render_related_paths_section(&related_paths));

    let actions = build_issue_detail_actions(issue, owner, repo_name);
    let mut hints = vec![presentation::text_navigation_hint(*selection)];
    hints.push(presentation::Hint::new(
        "Paths outside Actions are GET routes and are shown without a method prefix.",
    ));
    if is_pr {
        hints.push(presentation::Hint::new(
            "Pull request file lists summarize the comparison shown on the HTML detail page; full patch hunks remain HTML-only.",
        ));
    }

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    Ok(markdown)
}

fn render_issue_body(body: &str) -> String {
    if body.trim().is_empty() {
        r#"<p class="issue-no-description">No description provided.</p>"#.to_string()
    } else {
        format!(
            r#"<div class="issue-body readme-body">{}</div>"#,
            web::render_markdown(body)
        )
    }
}

fn issue_state_label(state: &str) -> &'static str {
    match state {
        "merged" => "Merged",
        "closed" => "Closed",
        _ => "Open",
    }
}

fn render_comment(
    author: &str,
    created_at: i64,
    body: &str,
    is_first: bool,
    _extra_class: &str,
) -> String {
    let _ = is_first;
    format!(
        r#"<div class="comment">
          <div class="comment-header">
            <strong>{author}</strong>
            <span class="comment-time">{time}</span>
          </div>
          <div class="comment-body readme-body">{body}</div>
        </div>"#,
        author = web::html_escape(author),
        time = web::format_time(created_at),
        body = web::render_markdown(body),
    )
}

fn render_state_button(
    issue: &issues::IssueRow,
    owner: &str,
    repo_name: &str,
    kind_url: &str,
    viewer: Viewer<'_>,
) -> String {
    let actor = match viewer.name {
        Some(actor) => actor,
        None => return String::new(),
    };
    if issue.state == "merged" {
        return String::new();
    }
    // Same rule as the close/reopen routes: the author, or triage and above.
    if actor != issue.author_name && viewer.role < Role::Triage {
        return String::new();
    }
    if issue.state == "open" {
        format!(
            r#"<button type="submit"
                formaction="/{owner}/{repo}/{kind}/{num}/close"
                class="btn-action">Close</button>"#,
            owner = web::html_escape(owner),
            repo = web::html_escape(repo_name),
            kind = kind_url,
            num = issue.number,
        )
    } else {
        format!(
            r#"<button type="submit"
                formaction="/{owner}/{repo}/{kind}/{num}/reopen"
                class="btn-action">Reopen</button>"#,
            owner = web::html_escape(owner),
            repo = web::html_escape(repo_name),
            kind = kind_url,
            num = issue.number,
        )
    }
}

fn render_pr_diff_section(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    issue: &issues::IssueRow,
    viewer: Viewer<'_>,
) -> Result<String> {
    let target_branch = match &issue.target_branch {
        Some(branch) => branch.as_str(),
        None => return Ok(String::new()),
    };

    let diff_data = match load_pr_diff_data(sql, issue)? {
        Some(diff_data) => diff_data,
        None => return Ok(String::new()),
    };

    let (files, stats) = match diff_data {
        PrDiffData::SourceMissing { source_branch } => {
            return Ok(format!(
                r#"<div class="pr-diff-section">
                      <p class="pr-branch-missing">Branch <code>{}</code> no longer exists.</p>
                    </div>"#,
                web::html_escape(&source_branch)
            ));
        }
        PrDiffData::TargetMissing { target_branch } => {
            return Ok(format!(
                r#"<div class="pr-diff-section">
                      <p class="pr-branch-missing">Target branch <code>{}</code> not found.</p>
                    </div>"#,
                web::html_escape(&target_branch)
            ));
        }
        PrDiffData::Compared { files, stats } => (files, stats),
    };

    let stats_html = format!(
        r#"<div class="pr-diff-stats">
          <span>{} file{} changed</span>
          <span class="stat-add">+{}</span>
          <span class="stat-del">-{}</span>
        </div>"#,
        stats.files_changed,
        if stats.files_changed == 1 { "" } else { "s" },
        stats.additions,
        stats.deletions,
    );

    let mut files_html = String::new();
    for file in &files {
        files_html.push_str(&crate::web::render_file_diff(file));
    }

    let merge_section = if issue.state == "open" && viewer.can_write() {
        format!(
            r#"<div class="pr-merge-box">
              <form method="POST"
                    action="/{owner}/{repo}/pulls/{num}/merge">
                <button type="submit" class="btn-merge">Merge pull request</button>
              </form>
              <p class="pr-merge-hint">
                Creates a merge commit on <code>{target}</code>.
                Conflicts return a 409 error.
              </p>
            </div>"#,
            owner = web::html_escape(owner),
            repo = web::html_escape(repo_name),
            num = issue.number,
            target = web::html_escape(target_branch),
        )
    } else if issue.state == "merged" {
        let merge_hash = issue.merge_commit_hash.as_deref().unwrap_or("");
        format!(
            r#"<div class="pr-merged-box">
              PR merged in <a href="/{owner}/{repo}/commit/{hash}">{short}</a>.
            </div>"#,
            owner = web::html_escape(owner),
            repo = web::html_escape(repo_name),
            hash = web::html_escape(merge_hash),
            short = web::html_escape(&merge_hash[..merge_hash.len().min(7)]),
        )
    } else {
        String::new()
    };

    Ok(format!(
        r#"<div class="pr-diff-section">
          <h2>Files changed</h2>
          {stats_html}
          {merge_section}
          {files_html}
        </div>"#,
        stats_html = stats_html,
        merge_section = merge_section,
        files_html = files_html,
    ))
}

fn render_pr_markdown_section(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    issue: &issues::IssueRow,
    viewer: Viewer<'_>,
) -> Result<String> {
    let mut markdown = String::from("\n## Pull Request\n");

    match (&issue.source_branch, &issue.target_branch) {
        (Some(source_branch), Some(target_branch)) => {
            markdown.push_str(&format!(
                "- Branches: `{}` -> `{}`\n",
                source_branch, target_branch
            ));
        }
        _ => markdown.push_str("- Branches: unavailable\n"),
    }

    if let Some(source_hash) = &issue.source_hash {
        markdown.push_str(&format!(
            "- Source head at creation: `{}`\n",
            &source_hash[..source_hash.len().min(12)]
        ));
    }

    markdown.push_str(&format!(
        "- Merge status: {}\n",
        render_merge_status_line(owner, repo_name, issue, viewer)
    ));

    markdown.push_str("\n## Changed Files\n");
    match load_pr_diff_data(sql, issue)? {
        Some(PrDiffData::SourceMissing { source_branch }) => {
            markdown.push_str(&format!(
                "Source branch `{}` no longer exists.\n",
                source_branch
            ));
        }
        Some(PrDiffData::TargetMissing { target_branch }) => {
            markdown.push_str(&format!(
                "Target branch `{}` was not found.\n",
                target_branch
            ));
        }
        Some(PrDiffData::Compared { files, stats }) => {
            markdown.push_str(&format!(
                "- Files changed: `{}`\n- Additions: `+{}`\n- Deletions: `-{}`\n",
                stats.files_changed, stats.additions, stats.deletions
            ));
            if files.is_empty() {
                markdown.push_str("\nNo changed files detected.\n");
            } else {
                markdown.push('\n');
                for file in &files {
                    markdown.push_str(&format!(
                        "- {} `{}`\n",
                        diff_status_label(&file.status),
                        file.path
                    ));
                }
            }
        }
        None => markdown.push_str("Changed file summary is unavailable for this pull request.\n"),
    }

    Ok(markdown)
}

fn render_merge_status_line(
    owner: &str,
    repo_name: &str,
    issue: &issues::IssueRow,
    viewer: Viewer<'_>,
) -> String {
    if issue.state == "merged" {
        if let Some(merge_hash) = &issue.merge_commit_hash {
            return format!(
                "merged in `{}`",
                format!("/{}/{}/commit/{}", owner, repo_name, merge_hash)
            );
        }
        return "merged; merge commit record unavailable".to_string();
    }

    if issue.state == "closed" {
        return "closed without merge".to_string();
    }

    let merge_path = format!("/{}/{}/pulls/{}/merge", owner, repo_name, issue.number);
    let target_branch = issue
        .target_branch
        .as_deref()
        .unwrap_or("the target branch");
    if viewer.can_write() {
        format!(
            "open; you can `POST {}` to create a merge commit on `{}`; conflicts return `409`",
            merge_path, target_branch
        )
    } else {
        format!(
            "open; someone with write access can merge with `POST {}` to create a merge commit on `{}`",
            merge_path, target_branch
        )
    }
}

fn load_pr_diff_data(sql: &SqlStorage, issue: &issues::IssueRow) -> Result<Option<PrDiffData>> {
    let source_branch = match &issue.source_branch {
        Some(branch) => branch.as_str(),
        None => return Ok(None),
    };
    let target_branch = match &issue.target_branch {
        Some(branch) => branch.as_str(),
        None => return Ok(None),
    };

    if issue.state == "merged" {
        if let Some(ref merge_hash) = issue.merge_commit_hash {
            let diff_result = diff::diff_commit(sql, merge_hash, true, 3)?;
            return Ok(Some(PrDiffData::Compared {
                files: diff_result.files,
                stats: diff_result.stats,
            }));
        }
        return Ok(None);
    }

    let source_ref = format!("refs/heads/{}", source_branch);
    let target_ref = format!("refs/heads/{}", target_branch);

    let source_hash = match api::resolve_ref(sql, &source_ref)? {
        Some(hash) => hash,
        None => {
            return Ok(Some(PrDiffData::SourceMissing {
                source_branch: source_branch.to_string(),
            }));
        }
    };
    let target_hash = match api::resolve_ref(sql, &target_ref)? {
        Some(hash) => hash,
        None => {
            return Ok(Some(PrDiffData::TargetMissing {
                target_branch: target_branch.to_string(),
            }));
        }
    };

    let base_hash = issues::find_merge_base(sql, &source_hash, &target_hash)?
        .unwrap_or_else(|| target_hash.clone());
    let comparison = diff::compare(sql, &base_hash, &source_hash, true, 3)?;
    Ok(Some(PrDiffData::Compared {
        files: comparison.files,
        stats: comparison.stats,
    }))
}

fn diff_status_label(status: &diff::DiffStatus) -> &'static str {
    match status {
        diff::DiffStatus::Added => "added",
        diff::DiffStatus::Deleted => "deleted",
        diff::DiffStatus::Modified => "modified",
    }
}

fn render_related_paths_section(paths: &[(&str, String)]) -> String {
    let mut markdown = String::from("\n## Related Paths\n");
    for (label, path) in paths {
        markdown.push_str(&format!("- {}: `{}`\n", label, path));
    }
    markdown
}

fn build_issue_detail_actions(
    issue: &issues::IssueRow,
    owner: &str,
    repo_name: &str,
) -> Vec<presentation::Action> {
    let kind_url = if issue.kind == "pr" {
        "pulls"
    } else {
        "issues"
    };
    let base_path = format!("/{}/{}/{}/{}", owner, repo_name, kind_url, issue.number);
    let mut actions = vec![presentation::Action::post(
        format!("{}/comment", base_path),
        "add a comment to this thread",
    )
    .with_fields(vec![presentation::ActionField::required(
        "body",
        "markdown or plain text comment body",
    )])
    .with_requires("authenticated user")
    .with_effect("appends a new comment and updates the thread timestamp")];

    match issue.state.as_str() {
        "open" => {
            actions.push(
                presentation::Action::post(format!("{}/close", base_path), "close this item")
                    .with_requires("issue or pull author, or repo owner")
                    .with_effect("marks the item closed"),
            );
            if issue.kind == "pr" {
                let target_branch = issue
                    .target_branch
                    .as_deref()
                    .unwrap_or("the target branch");
                actions.push(
                    presentation::Action::post(
                        format!("{}/merge", base_path),
                        "merge this pull request",
                    )
                    .with_requires("repo owner and a conflict-free merge")
                    .with_effect(format!(
                        "creates a merge commit on `{}` and marks the pull request merged; conflicts return `409`",
                        target_branch
                    )),
                );
            }
        }
        "closed" => {
            actions.push(
                presentation::Action::post(format!("{}/reopen", base_path), "reopen this item")
                    .with_requires("issue or pull author, or repo owner")
                    .with_effect("marks the item open again"),
            );
        }
        _ => {}
    }

    actions
}

fn markdown_literal_block(text: &str) -> String {
    let mut output = String::new();
    for line in text.lines() {
        output.push_str("    ");
        output.push_str(line);
        output.push('\n');
    }
    if output.is_empty() {
        output.push_str("    \n");
    }
    output
}
