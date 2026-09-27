use super::*;

const MAX_HUNKS_PER_FILE: usize = 8;
const MAX_LINES_PER_HUNK: usize = 18;

struct CommitPage {
    owner: String,
    repo_name: String,
    hash: String,
    default_branch: String,
    commit: CommitMeta,
    diff_result: diff::CommitDiff,
}

enum CommitMarkdownRoute {
    Commit,
    Diff,
}

impl CommitPage {
    fn short_hash(&self) -> &str {
        &self.hash[..7.min(self.hash.len())]
    }

    fn subject(&self) -> String {
        first_line(&self.commit.message)
    }

    fn body(&self) -> String {
        rest_of_message(&self.commit.message)
    }

    fn commit_path(&self, hash: &str) -> String {
        format!("/{}/{}/commit/{}", self.owner, self.repo_name, hash)
    }

    fn diff_path(&self, hash: &str) -> String {
        format!("/{}/{}/diff/{}", self.owner, self.repo_name, hash)
    }
}

fn build_commit_page(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    hash: &str,
) -> Result<CommitPage> {
    if hash.is_empty() {
        return Err(Error::RustError("missing commit hash".into()));
    }

    let (default_branch, _) = resolve_default_branch(sql)?;
    let commit = load_commit_meta(sql, hash)?;
    let diff_result = diff::diff_commit(sql, hash, true, 3)?;

    Ok(CommitPage {
        owner: owner.to_string(),
        repo_name: repo_name.to_string(),
        hash: hash.to_string(),
        default_branch,
        commit,
        diff_result,
    })
}

fn render_commit_html(page: &CommitPage, viewer: Viewer<'_>) -> String {
    let mut html = String::new();
    html.push_str(&format!(
        r#"<h1 style="font-size:18px;margin-bottom:4px">{msg}</h1>"#,
        msg = html_escape(&page.subject()),
    ));
    html.push_str(&format!(
        r#"<p style="color:#656d76;margin-bottom:16px">{author} &lt;{email}&gt; committed {time}</p>"#,
        author = html_escape(&page.commit.author),
        email = html_escape(&page.commit.author_email),
        time = format_time(page.commit.commit_time),
    ));

    let rest = page.body();
    if !rest.is_empty() {
        html.push_str(&format!(
            r#"<pre style="margin-bottom:16px;padding:12px;background:#f6f8fa;border-radius:6px;white-space:pre-wrap">{}</pre>"#,
            html_escape(&rest),
        ));
    }

    html.push_str(&format!(
        r#"<div style="font-family:monospace;font-size:13px;margin-bottom:16px;color:#656d76">
  commit {hash}<br>
  {parents}
</div>"#,
        hash = page.hash,
        parents = if let Some(ref parent_hash) = page.diff_result.parent_hash {
            format!(
                r#"parent <a href="/{}/{}/commit/{}">{}</a>"#,
                page.owner,
                page.repo_name,
                parent_hash,
                &parent_hash[..7.min(parent_hash.len())]
            )
        } else {
            "root commit".to_string()
        },
    ));

    html.push_str(&render_stats_html(&page.diff_result.stats));

    for file in &page.diff_result.files {
        html.push_str(&render_file_diff(file));
    }

    layout(
        &format!("Commit {}", page.short_hash()),
        &page.owner,
        &page.repo_name,
        &page.default_branch,
        viewer,
        &html,
    )
}

fn render_stats_html(stats: &diff::DiffStats) -> String {
    format!(
        r#"<div class="diff-stats">
  Showing <strong>{files}</strong> changed file{s} with
  <span class="stat-add">+{add}</span> addition{as_} and
  <span class="stat-del">-{del}</span> deletion{ds}.
</div>"#,
        files = stats.files_changed,
        s = plural_suffix(stats.files_changed),
        add = stats.additions,
        as_ = plural_suffix(stats.additions),
        del = stats.deletions,
        ds = plural_suffix(stats.deletions),
    )
}

fn render_commit_markdown(
    page: &CommitPage,
    route: CommitMarkdownRoute,
    selection: &NegotiatedRepresentation,
) -> String {
    let route_path = match route {
        CommitMarkdownRoute::Commit => page.commit_path(&page.hash),
        CommitMarkdownRoute::Diff => page.diff_path(&page.hash),
    };
    let route_label = match route {
        CommitMarkdownRoute::Commit => "commit",
        CommitMarkdownRoute::Diff => "diff",
    };
    let mut markdown = format!(
        "# {}/{} {} `{}`\n\nSubject: {}\nAuthor: {} <{}>\nCommitted: {}\nCommit: `{}`\n",
        page.owner,
        page.repo_name,
        route_label,
        page.short_hash(),
        page.subject(),
        page.commit.author,
        page.commit.author_email,
        format_time(page.commit.commit_time),
        page.hash,
    );

    if let Some(parent_hash) = &page.diff_result.parent_hash {
        markdown.push_str(&format!(
            "Parent: `{}` - `{}`\n",
            &parent_hash[..7.min(parent_hash.len())],
            page.commit_path(parent_hash)
        ));
    } else {
        markdown.push_str("Parent: root commit\n");
    }

    markdown.push_str(&format!(
        "Stats: {} file{} changed, +{}, -{}\n",
        page.diff_result.stats.files_changed,
        plural_suffix(page.diff_result.stats.files_changed),
        page.diff_result.stats.additions,
        page.diff_result.stats.deletions,
    ));

    let body = page.body();
    if !body.is_empty() {
        markdown.push_str("\n## Message\n\n");
        markdown.push_str(&markdown_literal_block(&body));
    }

    markdown.push_str("\n## Changed Files\n");
    if page.diff_result.files.is_empty() {
        markdown.push_str("No file changes.\n");
    } else {
        for file in &page.diff_result.files {
            markdown.push_str(&format!(
                "- `{}` `{}` - +{}, -{}, {}\n",
                diff_status_letter(&file.status),
                file.path,
                file_additions(file),
                file_deletions(file),
                hunk_summary(file)
            ));
        }
    }

    let detail_sections = page
        .diff_result
        .files
        .iter()
        .filter_map(render_file_markdown_details)
        .collect::<Vec<_>>();
    if !detail_sections.is_empty() {
        markdown.push_str("\n## Diff Details\n");
        for section in detail_sections {
            markdown.push_str(&section);
        }
    }

    let commit_json_path = presentation::append_format(
        &page.commit_path(&page.hash),
        presentation::Representation::Json,
    );
    let diff_json_path = presentation::append_format(
        &page.diff_path(&page.hash),
        presentation::Representation::Json,
    );

    let mut actions = vec![
        Action::get(route_path, format!("reload this {} page", route_label)),
        Action::get(page.commit_path(&page.hash), "open the commit route"),
        Action::get(page.diff_path(&page.hash), "open the diff route"),
        Action::get(commit_json_path, "fetch the structured commit record"),
        Action::get(diff_json_path, "fetch the structured diff with hunks"),
    ];

    if let Some(parent_hash) = &page.diff_result.parent_hash {
        actions.push(Action::get(
            page.commit_path(parent_hash),
            "inspect the parent commit",
        ));
    }

    let mut hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new("`/commit/:hash` and `/diff/:hash` can share this same markdown page model; the JSON endpoints differ."),
        Hint::new(format!(
            "Use `{}?context=N&format=json` for more or less diff context, or add `&stat=1` for stats only.",
            page.diff_path(&page.hash)
        )),
    ];
    if page.diff_result.parent_hash.is_none() {
        hints.push(Hint::new(
            "Root commits have no parent navigation target; every listed file is introduced here.",
        ));
    }

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    markdown
}

fn render_file_markdown_details(file: &diff::FileDiff) -> Option<String> {
    let hunks = file.hunks.as_ref()?;
    let mut section = String::new();
    section.push_str(&format!(
        "\n### `{}` `{}`\n\n",
        diff_status_letter(&file.status),
        file.path
    ));
    section.push_str(&format!(
        "Summary: +{}, -{}, {}\n\n",
        file_additions(file),
        file_deletions(file),
        hunk_summary(file)
    ));

    for (idx, hunk) in hunks.iter().enumerate() {
        if idx >= MAX_HUNKS_PER_FILE {
            section.push_str(&format!(
                "{} more hunk{} omitted.\n\n",
                hunks.len() - MAX_HUNKS_PER_FILE,
                plural_suffix(hunks.len() - MAX_HUNKS_PER_FILE)
            ));
            break;
        }

        if hunk.lines.len() == 1 && hunk.lines[0].tag == "binary" {
            section.push_str("    Binary files differ\n\n");
            continue;
        }

        section.push_str(&format!(
            "    @@ -{},{} +{},{} @@\n",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ));

        for (line_idx, line) in hunk.lines.iter().enumerate() {
            if line_idx >= MAX_LINES_PER_HUNK {
                section.push_str(&format!(
                    "    ... {} more line{} omitted ...\n",
                    hunk.lines.len() - MAX_LINES_PER_HUNK,
                    plural_suffix(hunk.lines.len() - MAX_LINES_PER_HUNK)
                ));
                break;
            }

            let prefix = match line.tag {
                "add" => '+',
                "delete" => '-',
                "binary" => '!',
                _ => ' ',
            };
            section.push_str("    ");
            section.push(prefix);
            section.push_str(line.content.trim_end_matches('\n'));
            section.push('\n');
        }
        section.push('\n');
    }

    Some(section)
}

fn diff_status_letter(status: &diff::DiffStatus) -> &'static str {
    match status {
        diff::DiffStatus::Added => "A",
        diff::DiffStatus::Deleted => "D",
        diff::DiffStatus::Modified => "M",
    }
}

fn file_additions(file: &diff::FileDiff) -> usize {
    file.hunks
        .as_ref()
        .map(|hunks| {
            hunks
                .iter()
                .flat_map(|hunk| hunk.lines.iter())
                .filter(|line| line.tag == "add")
                .count()
        })
        .unwrap_or_else(|| match file.status {
            diff::DiffStatus::Added => 1,
            diff::DiffStatus::Deleted => 0,
            diff::DiffStatus::Modified => 1,
        })
}

fn file_deletions(file: &diff::FileDiff) -> usize {
    file.hunks
        .as_ref()
        .map(|hunks| {
            hunks
                .iter()
                .flat_map(|hunk| hunk.lines.iter())
                .filter(|line| line.tag == "delete")
                .count()
        })
        .unwrap_or_else(|| match file.status {
            diff::DiffStatus::Added => 0,
            diff::DiffStatus::Deleted => 1,
            diff::DiffStatus::Modified => 1,
        })
}

fn hunk_summary(file: &diff::FileDiff) -> String {
    match &file.hunks {
        Some(hunks) => format!("{} hunk{}", hunks.len(), plural_suffix(hunks.len())),
        None => "no hunk detail".to_string(),
    }
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
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

pub fn page_commit(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    hash: &str,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let page = build_commit_page(sql, owner, repo_name, hash)?;
    html_response(&render_commit_html(&page, viewer))
}

pub fn page_commit_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    hash: &str,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let page = build_commit_page(sql, owner, repo_name, hash)?;
    presentation::markdown_response(
        &render_commit_markdown(&page, CommitMarkdownRoute::Commit, selection),
        selection,
    )
}

pub fn page_diff_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    hash: &str,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let page = build_commit_page(sql, owner, repo_name, hash)?;
    presentation::markdown_response(
        &render_commit_markdown(&page, CommitMarkdownRoute::Diff, selection),
        selection,
    )
}

fn rest_of_message(message: &str) -> String {
    let mut lines = message.lines();
    lines.next();
    let rest: String = lines.collect::<Vec<_>>().join("\n");
    rest.trim().to_string()
}
