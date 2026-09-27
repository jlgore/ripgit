use super::*;

struct HomeTreeEntry {
    name: String,
    path: String,
    is_tree: bool,
}

struct HomeReadme {
    name: String,
    path: String,
    content: String,
    is_markdown: bool,
}

enum HomePageState {
    Populated {
        commit_hash: String,
        entries: Vec<HomeTreeEntry>,
        recent_commits: Vec<CommitListItem>,
        readme: Option<HomeReadme>,
    },
    Empty,
}

struct HomePage {
    route_path: String,
    owner: String,
    repo_name: String,
    ref_name: String,
    branches: Vec<String>,
    viewer_is_owner: bool,
    scheme: String,
    host: String,
    state: HomePageState,
}

impl HomePage {
    fn home_path(&self, ref_name: &str) -> String {
        format!("{}?ref={}", self.route_path, ref_name)
    }

    fn current_path(&self) -> String {
        self.home_path(&self.ref_name)
    }

    fn tree_path(&self, path: &str) -> String {
        format!(
            "/{}/{}/tree/{}/{}",
            self.owner, self.repo_name, self.ref_name, path
        )
    }

    fn blob_path(&self, path: &str) -> String {
        format!(
            "/{}/{}/blob/{}/{}",
            self.owner, self.repo_name, self.ref_name, path
        )
    }

    fn commits_path(&self) -> String {
        format!(
            "/{}/{}/commits?ref={}",
            self.owner, self.repo_name, self.ref_name
        )
    }

    fn search_path(&self) -> String {
        format!("/{}/{}/search-ui?scope=code", self.owner, self.repo_name)
    }

    fn issues_path(&self) -> String {
        format!("/{}/{}/issues", self.owner, self.repo_name)
    }

    fn pulls_path(&self) -> String {
        format!("/{}/{}/pulls", self.owner, self.repo_name)
    }

    fn new_issue_path(&self) -> String {
        format!("/{}/{}/issues/new", self.owner, self.repo_name)
    }

    fn new_pull_path(&self) -> String {
        format!("/{}/{}/pulls/new", self.owner, self.repo_name)
    }

    fn settings_path(&self) -> String {
        format!("/{}/{}/settings", self.owner, self.repo_name)
    }

    fn push_remote_url(&self) -> String {
        format!(
            "{}://{}:TOKEN@{}/{}/{}",
            self.scheme, self.owner, self.host, self.owner, self.repo_name
        )
    }
}

fn build_home_page(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    viewer: Viewer<'_>,
) -> Result<HomePage> {
    let (ref_name, head_hash) = if let Some(reference) = api::get_query(url, "ref") {
        let hash = api::resolve_ref(sql, &reference)?;
        (reference, hash)
    } else {
        resolve_default_branch(sql)?
    };

    let state = match head_hash {
        Some(hash) => {
            let tree_hash = load_tree_for_commit(sql, &hash)?;
            let entries = load_sorted_tree(sql, &tree_hash)?
                .into_iter()
                .map(|entry| HomeTreeEntry {
                    path: entry.name.clone(),
                    name: entry.name,
                    is_tree: entry.is_tree,
                })
                .collect();
            let recent_commits = summarize_commits(walk_commits(sql, &hash, 5, 0)?);
            let readme = load_root_readme(sql, &tree_hash)?;
            HomePageState::Populated {
                commit_hash: hash,
                entries,
                recent_commits,
                readme,
            }
        }
        None => HomePageState::Empty,
    };

    Ok(HomePage {
        route_path: url.path().to_string(),
        owner: owner.to_string(),
        repo_name: repo_name.to_string(),
        ref_name,
        branches: load_branches(sql)?,
        viewer_is_owner: viewer.can_write(),
        scheme: url.scheme().to_string(),
        host: url.host_str().unwrap_or("your-worker.dev").to_string(),
        state,
    })
}

fn render_home_branch_selector(page: &HomePage) -> String {
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
            html_escape(&page.home_path(branch)),
            selected,
            html_escape(branch)
        ));
    }

    html.push_str("</select></div>");
    html
}

fn render_home_html(page: &HomePage, viewer: Viewer<'_>, sql: &SqlStorage) -> String {
    let content = match &page.state {
        HomePageState::Populated {
            commit_hash,
            entries,
            recent_commits,
            readme,
        } => {
            let mut html = String::new();
            html.push_str(&render_home_branch_selector(page));

            html.push_str(r#"<table class="tree-table">"#);
            for entry in entries {
                let icon = if entry.is_tree {
                    "&#128193;"
                } else {
                    "&#128196;"
                };
                let link = if entry.is_tree {
                    page.tree_path(&entry.path)
                } else {
                    page.blob_path(&entry.path)
                };
                html.push_str(&format!(
                    r#"<tr><td class="tree-icon">{icon}</td><td class="tree-name"><a href="{link}">{name}</a></td></tr>"#,
                    icon = icon,
                    link = html_escape(&link),
                    name = html_escape(&entry.name),
                ));
            }
            html.push_str("</table>");

            html.push_str(r#"<h2 style="margin-top:24px">Recent commits</h2>"#);
            html.push_str(&render_commit_list(
                recent_commits,
                &page.owner,
                &page.repo_name,
                false,
            ));

            if let Some(readme) = readme {
                html.push_str(&render_home_readme_html(sql, page, commit_hash, readme));
            }

            html
        }
        HomePageState::Empty => {
            if page.viewer_is_owner {
                format!(
                    r#"<div class="empty-repo">
<h2>This repository is empty</h2>
<p>Push your first commit to get started:</p>
<pre class="push-cmd">cd my-project
git init
git add .
git commit -m "initial commit"
git remote add origin {remote}
git push origin main</pre>
<p class="push-note">Replace <code>TOKEN</code> with an access token from <a href="/settings">Settings</a>.</p>
</div>"#,
                    remote = page.push_remote_url(),
                )
            } else {
                "<p class=\"empty-repo-msg\">Empty repository.</p>".to_string()
            }
        }
    };

    layout(
        "Home",
        &page.owner,
        &page.repo_name,
        &page.ref_name,
        viewer,
        &content,
    )
}

fn render_home_markdown(page: &HomePage, selection: &NegotiatedRepresentation) -> String {
    let mut markdown = format!(
        "# {}/{}\n\nBranch: `{}`\n",
        page.owner, page.repo_name, page.ref_name
    );

    match &page.state {
        HomePageState::Populated {
            commit_hash: _,
            entries,
            recent_commits,
            readme,
        } => {
            markdown.push_str("\n## Files (GET paths)\n");
            if entries.is_empty() {
                markdown.push_str("No files at this ref.\n");
            } else {
                for entry in entries {
                    let label = if entry.is_tree { "dir" } else { "file" };
                    let display_name = if entry.is_tree {
                        format!("{}/", entry.name)
                    } else {
                        entry.name.clone()
                    };
                    let path = if entry.is_tree {
                        page.tree_path(&entry.path)
                    } else {
                        page.blob_path(&entry.path)
                    };
                    markdown.push_str(&format!("- {} `{}` - `{}`\n", label, display_name, path));
                }
            }

            markdown.push_str("\n## Recent Commits (GET paths)\n");
            if recent_commits.is_empty() {
                markdown.push_str("No commits yet.\n");
            } else {
                for commit in recent_commits {
                    markdown.push_str(&format!(
                        "- `{}` - {} - {} - {} - `{}`\n",
                        commit.short_hash,
                        commit.subject,
                        commit.author,
                        commit.relative_time,
                        format!("/{}/{}/commit/{}", page.owner, page.repo_name, commit.hash)
                    ));
                }
            }

            if let Some(readme) = readme {
                markdown.push_str(&format!(
                    "\n## README (`{}`)\nSource path: `{}`\n",
                    readme.name,
                    page.blob_path(&readme.path)
                ));
                if readme.is_markdown {
                    markdown.push_str("Format: source markdown\n");
                }
                markdown.push('\n');
                markdown.push_str(&markdown_literal_block(&readme.content));
            }
        }
        HomePageState::Empty => {
            markdown.push_str("\nRepository is empty.\n");
            if page.viewer_is_owner {
                markdown.push_str("\nPush your first commit with:\n\n");
                markdown.push_str(&markdown_literal_block(&format!(
                    "cd my-project\ngit init\ngit add .\ngit commit -m \"initial commit\"\ngit remote add origin {}\ngit push origin main",
                    page.push_remote_url()
                )));
                markdown.push_str("\nReplace `TOKEN` with an access token from `/settings`.\n");
            }
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
                page.home_path(branch)
            ));
        }
    }

    let mut actions = vec![
        Action::get(page.current_path(), "reload this repository view"),
        Action::get(page.issues_path(), "list issues"),
        Action::get(page.pulls_path(), "list pull requests"),
        Action::get(page.search_path(), "search the default branch code index"),
        Action::get(page.new_issue_path(), "open the new issue form")
            .with_requires("authenticated user"),
        Action::get(page.new_pull_path(), "open the new pull request form")
            .with_requires("authenticated user"),
        Action::get(page.settings_path(), "open repository settings").with_requires("repo owner"),
    ];

    if matches!(page.state, HomePageState::Populated { .. }) {
        actions.insert(
            1,
            Action::get(
                page.commits_path(),
                "browse the full commit history for this ref",
            ),
        );
    }

    let hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new("Directories use `/tree/{ref}/{path}` and files use `/blob/{ref}/{path}`."),
        Hint::new(
            "Search currently follows the default branch index rather than the `ref=` query.",
        ),
    ];

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    markdown
}

fn load_root_readme(sql: &SqlStorage, tree_hash: &str) -> Result<Option<HomeReadme>> {
    let entries = load_sorted_tree(sql, tree_hash)?;
    let readme = entries.iter().find(|entry| {
        let lower = entry.name.to_lowercase();
        lower == "readme.md" || lower == "readme" || lower == "readme.txt"
    });

    let entry = match readme {
        Some(entry) => entry,
        None => return Ok(None),
    };

    if entry.is_tree {
        return Ok(None);
    }

    let content = load_blob(sql, &entry.hash)?;
    let text = match std::str::from_utf8(&content) {
        Ok(text) => text.to_string(),
        Err(_) => return Ok(None),
    };

    Ok(Some(HomeReadme {
        name: entry.name.clone(),
        path: entry.name.clone(),
        is_markdown: entry.name.to_lowercase().ends_with(".md"),
        content: text,
    }))
}

fn render_home_readme_html(
    sql: &SqlStorage,
    page: &HomePage,
    commit_hash: &str,
    readme: &HomeReadme,
) -> String {
    let mut html = String::new();
    html.push_str(r#"<div class="readme-box">"#);
    html.push_str(&format!(
        r#"<div class="readme-header"><a href="{}">{}</a></div>"#,
        html_escape(&page.blob_path(&readme.path)),
        html_escape(&readme.name),
    ));
    html.push_str(r#"<div class="readme-body">"#);

    if readme.is_markdown {
        html.push_str(&render_repo_markdown(
            sql,
            &readme.content,
            &page.owner,
            &page.repo_name,
            &page.ref_name,
            commit_hash,
            &readme.path,
        ));
    } else {
        html.push_str(&format!("<pre>{}</pre>", html_escape(&readme.content)));
    }

    html.push_str("</div></div>");
    html
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

pub fn page_home(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let page = build_home_page(sql, owner, repo_name, url, viewer)?;
    html_response(&render_home_html(&page, viewer, sql))
}

pub fn page_home_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    url: &Url,
    viewer: Viewer<'_>,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let page = build_home_page(sql, owner, repo_name, url, viewer)?;
    presentation::markdown_response(&render_home_markdown(&page, selection), selection)
}
