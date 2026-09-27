use super::*;

struct TreePage {
    owner: String,
    repo_name: String,
    ref_name: String,
    path: String,
    commit_hash: String,
    branches: Vec<String>,
    entries: Vec<SortedEntry>,
}

impl TreePage {
    fn current_path(&self) -> String {
        format!(
            "/{}/{}/tree/{}/{}",
            self.owner, self.repo_name, self.ref_name, self.path
        )
    }

    fn repo_home_path(&self) -> String {
        format!("/{}/{}/?ref={}", self.owner, self.repo_name, self.ref_name)
    }

    fn commits_path(&self) -> String {
        format!(
            "/{}/{}/commits?ref={}",
            self.owner, self.repo_name, self.ref_name
        )
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

    fn display_path(&self) -> &str {
        if self.path.is_empty() {
            "/"
        } else {
            &self.path
        }
    }

    fn up_path(&self) -> Option<String> {
        if self.path.is_empty() {
            return None;
        }

        let parent = parent_path(&self.path);
        Some(if parent.is_empty() {
            format!("/{}/{}/", self.owner, self.repo_name)
        } else {
            self.tree_path(&parent)
        })
    }

    fn child_path(&self, entry: &SortedEntry) -> String {
        let full_path = if self.path.is_empty() {
            entry.name.clone()
        } else {
            format!("{}/{}", self.path, entry.name)
        };
        if entry.is_tree {
            self.tree_path(&full_path)
        } else {
            self.blob_path(&full_path)
        }
    }

    fn breadcrumb_paths(&self) -> Vec<(String, String, bool)> {
        let mut crumbs = vec![("/".to_string(), self.repo_home_path(), self.path.is_empty())];

        if self.path.is_empty() {
            return crumbs;
        }

        let parts: Vec<&str> = self
            .path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        for (index, part) in parts.iter().enumerate() {
            let sub_path = parts[..=index].join("/");
            crumbs.push((
                (*part).to_string(),
                self.tree_path(&sub_path),
                index == parts.len() - 1,
            ));
        }

        crumbs
    }

    fn branch_path(&self, branch: &str) -> String {
        format!(
            "/{}/{}/tree/{}/{}",
            self.owner, self.repo_name, branch, self.path
        )
    }
}

struct BlobPage {
    owner: String,
    repo_name: String,
    ref_name: String,
    path: String,
    commit_hash: String,
    blob_hash: String,
    branches: Vec<String>,
    content: Vec<u8>,
}

impl BlobPage {
    fn current_path(&self) -> String {
        format!(
            "/{}/{}/blob/{}/{}",
            self.owner, self.repo_name, self.ref_name, self.path
        )
    }

    fn repo_home_path(&self) -> String {
        format!("/{}/{}/?ref={}", self.owner, self.repo_name, self.ref_name)
    }

    fn commits_path(&self) -> String {
        format!(
            "/{}/{}/commits?ref={}",
            self.owner, self.repo_name, self.ref_name
        )
    }

    fn tree_path(&self, path: &str) -> String {
        format!(
            "/{}/{}/tree/{}/{}",
            self.owner, self.repo_name, self.ref_name, path
        )
    }

    fn raw_path(&self) -> String {
        format!(
            "/{}/{}/raw/{}/{}",
            self.owner, self.repo_name, self.ref_name, self.path
        )
    }

    fn filename(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    fn parent_tree_path(&self) -> String {
        let parent = parent_path(&self.path);
        if parent.is_empty() {
            format!("/{}/{}/", self.owner, self.repo_name)
        } else {
            self.tree_path(&parent)
        }
    }

    fn branch_path(&self, branch: &str) -> String {
        format!(
            "/{}/{}/blob/{}/{}",
            self.owner, self.repo_name, branch, self.path
        )
    }

    fn is_binary(&self) -> bool {
        let limit = self.content.len().min(8192);
        limit > 0 && self.content[..limit].contains(&0)
    }

    fn renderable_text(&self) -> Option<(String, bool)> {
        if self.is_binary() {
            return None;
        }

        match String::from_utf8(self.content.clone()) {
            Ok(text) => Some((text, true)),
            Err(_) => Some((String::from_utf8_lossy(&self.content).into_owned(), false)),
        }
    }

    fn breadcrumb_paths(&self) -> Vec<(String, String, bool)> {
        let mut crumbs = vec![("/".to_string(), self.repo_home_path(), false)];

        let parts: Vec<&str> = self
            .path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        for (index, part) in parts.iter().enumerate() {
            let is_last = index == parts.len() - 1;
            let sub_path = parts[..=index].join("/");
            let path = if is_last {
                self.current_path()
            } else {
                self.tree_path(&sub_path)
            };
            crumbs.push(((*part).to_string(), path, is_last));
        }

        crumbs
    }
}

fn build_tree_page(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    ref_name: &str,
    path: &str,
    commit_hash: String,
) -> Result<TreePage> {
    let tree_hash = resolve_path_to_tree(sql, &commit_hash, path)?;
    let entries = load_sorted_tree(sql, &tree_hash)?;

    Ok(TreePage {
        owner: owner.to_string(),
        repo_name: repo_name.to_string(),
        ref_name: ref_name.to_string(),
        path: path.to_string(),
        commit_hash,
        branches: load_branches(sql)?,
        entries,
    })
}

fn build_blob_page(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    ref_name: &str,
    path: &str,
    commit_hash: String,
) -> Result<BlobPage> {
    let blob_hash = resolve_path_to_blob(sql, &commit_hash, path)?;
    let content = load_blob(sql, &blob_hash)?;

    Ok(BlobPage {
        owner: owner.to_string(),
        repo_name: repo_name.to_string(),
        ref_name: ref_name.to_string(),
        path: path.to_string(),
        commit_hash,
        blob_hash,
        branches: load_branches(sql)?,
        content,
    })
}

fn render_tree_html(
    page: &TreePage,
    viewer: Viewer<'_>,
    sql: &SqlStorage,
) -> Result<Response> {
    let mut html = String::new();
    html.push_str(&render_branch_selector(
        sql,
        &page.owner,
        &page.repo_name,
        &page.ref_name,
        "tree",
        &page.path,
    )?);
    html.push_str(&render_breadcrumb(
        &page.owner,
        &page.repo_name,
        &page.ref_name,
        &page.path,
    ));

    html.push_str(r#"<table class="tree-table">"#);
    if let Some(parent_link) = page.up_path() {
        html.push_str(&format!(
            r#"<tr><td class="tree-icon">&#128193;</td><td class="tree-name"><a href="{}">..</a></td></tr>"#,
            parent_link
        ));
    }

    for entry in &page.entries {
        let icon = if entry.is_tree {
            "&#128193;"
        } else {
            "&#128196;"
        };
        let link = page.child_path(entry);
        html.push_str(&format!(
            r#"<tr><td class="tree-icon">{icon}</td><td class="tree-name"><a href="{link}">{name}</a></td></tr>"#,
            icon = icon,
            link = link,
            name = html_escape(&entry.name),
        ));
    }
    html.push_str("</table>");

    html_response(&layout(
        &page.path,
        &page.owner,
        &page.repo_name,
        &page.ref_name,
        viewer,
        &html,
    ))
}

fn render_tree_markdown(page: &TreePage, selection: &NegotiatedRepresentation) -> String {
    let mut markdown = format!(
        "# {}/{} tree\n\nBranch: `{}`\nLocation: `{}`\nCurrent path: `{}`\nCommit: `{}`\n",
        page.owner,
        page.repo_name,
        page.ref_name,
        page.display_path(),
        page.current_path(),
        page.commit_hash,
    );

    markdown.push_str("\n## Breadcrumb (GET paths)\n");
    for (label, path, current) in page.breadcrumb_paths() {
        let suffix = if current { " (current)" } else { "" };
        markdown.push_str(&format!("- `{}`{} - `{}`\n", label, suffix, path));
    }

    if let Some(up_path) = page.up_path() {
        markdown.push_str("\n## Navigation (GET paths)\n");
        markdown.push_str(&format!("- up - `{}`\n", up_path));
        markdown.push_str(&format!(
            "- repository home - `{}`\n",
            page.repo_home_path()
        ));
    }

    markdown.push_str("\n## Files (GET paths)\n");
    if page.entries.is_empty() {
        markdown.push_str("Directory is empty.\n");
    } else {
        for entry in &page.entries {
            let kind = if entry.is_tree { "dir" } else { "file" };
            let name = if entry.is_tree {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            };
            markdown.push_str(&format!(
                "- {} `{}` - `{}`\n",
                kind,
                name,
                page.child_path(entry)
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
                page.branch_path(branch)
            ));
        }
    }

    let mut actions = vec![
        Action::get(page.current_path(), "reload this tree view"),
        Action::get(
            page.repo_home_path(),
            "open the repository home at this ref",
        ),
        Action::get(page.commits_path(), "browse commit history for this ref"),
    ];
    if let Some(up_path) = page.up_path() {
        actions.push(Action::get(up_path, "move up to the parent directory"));
    }

    let hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new("Root breadcrumb paths go to the repository home, matching the HTML breadcrumb."),
        Hint::new("Directory entries sort with subdirectories first, then files."),
    ];

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    markdown
}

fn render_blob_html(
    page: &BlobPage,
    viewer: Viewer<'_>,
    sql: &SqlStorage,
) -> Result<Response> {
    let mut html = String::new();
    html.push_str(&render_branch_selector(
        sql,
        &page.owner,
        &page.repo_name,
        &page.ref_name,
        "blob",
        &page.path,
    )?);
    html.push_str(&render_breadcrumb(
        &page.owner,
        &page.repo_name,
        &page.ref_name,
        &page.path,
    ));

    let filename = page.filename();
    let size = page.content.len();
    html.push_str(&format!(
        r#"<div class="file-header"><span>{filename}</span><div style="display:flex;gap:8px;align-items:center"><span style="color:#656d76;font-size:13px">{size} bytes</span><a href="/{owner}/{repo}/raw/{ref_name}/{path}" class="raw-btn">Raw</a></div></div>"#,
        owner = html_escape(&page.owner),
        repo = html_escape(&page.repo_name),
        ref_name = html_escape(&page.ref_name),
        path = page.path,
        filename = html_escape(filename),
        size = size,
    ));

    if page.is_binary() {
        html.push_str(r#"<div class="file-content"><pre>Binary file not shown.</pre></div>"#);
    } else {
        let text = String::from_utf8_lossy(&page.content);
        let lang_class = lang_from_filename(filename);
        html.push_str(&format!(
            r#"<div class="file-content"><pre><code class="{lang}">{code}</code></pre></div>"#,
            lang = lang_class,
            code = html_escape(&text),
        ));
    }

    html_response(&layout(
        filename,
        &page.owner,
        &page.repo_name,
        &page.ref_name,
        viewer,
        &html,
    ))
}

fn render_blob_markdown(page: &BlobPage, selection: &NegotiatedRepresentation) -> String {
    let is_binary = page.is_binary();
    let text = page.renderable_text();

    let mut markdown = format!(
        "# {}/{} blob\n\nBranch: `{}`\nFile: `{}`\nCurrent path: `{}`\nParent tree: `{}`\nRaw path: `{}`\nCommit: `{}`\nBlob: `{}`\nSize: `{}` bytes\nBinary: `{}`\n",
        page.owner,
        page.repo_name,
        page.ref_name,
        page.path,
        page.current_path(),
        page.parent_tree_path(),
        page.raw_path(),
        page.commit_hash,
        page.blob_hash,
        page.content.len(),
        if is_binary { "yes" } else { "no" },
    );

    markdown.push_str("\n## Breadcrumb (GET paths)\n");
    for (label, path, current) in page.breadcrumb_paths() {
        let suffix = if current { " (current)" } else { "" };
        markdown.push_str(&format!("- `{}`{} - `{}`\n", label, suffix, path));
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
                page.branch_path(branch)
            ));
        }
    }

    markdown.push_str("\n## Content\n");
    if is_binary {
        markdown
            .push_str("Binary file not shown in text mode. Use the raw path for exact bytes.\n");
    } else if let Some((text, is_utf8)) = text {
        let encoding_note = if is_utf8 {
            "UTF-8 text"
        } else {
            "lossy UTF-8 view of non-UTF-8 text bytes"
        };
        markdown.push_str(&format!("Rendering: {}\n\n", encoding_note));
        markdown.push_str(&markdown_literal_block(&text));
    }

    let actions = vec![
        Action::get(page.current_path(), "reload this blob view"),
        Action::get(page.parent_tree_path(), "open the containing directory"),
        Action::get(page.raw_path(), "download or stream the raw file bytes"),
        Action::get(page.commits_path(), "browse commit history for this ref"),
    ];

    let hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new(
            "Use the raw path for exact bytes, especially for binary files or original encodings.",
        ),
        Hint::new("Line anchors like `#L42` remain available on the HTML blob route."),
    ];

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    markdown
}

pub fn page_tree(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    ref_name: &str,
    path: &str,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let commit_hash = match api::resolve_ref(sql, ref_name)? {
        Some(hash) => hash,
        None => return Response::error("ref not found", 404),
    };
    let page = build_tree_page(sql, owner, repo_name, ref_name, path, commit_hash)?;
    render_tree_html(&page, viewer, sql)
}

pub fn page_tree_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    ref_name: &str,
    path: &str,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let commit_hash = match api::resolve_ref(sql, ref_name)? {
        Some(hash) => hash,
        None => return Response::error("ref not found", 404),
    };
    let page = build_tree_page(sql, owner, repo_name, ref_name, path, commit_hash)?;
    presentation::markdown_response(&render_tree_markdown(&page, selection), selection)
}

pub fn page_blob(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    ref_name: &str,
    path: &str,
    viewer: Viewer<'_>,
) -> Result<Response> {
    let commit_hash = match api::resolve_ref(sql, ref_name)? {
        Some(hash) => hash,
        None => return Response::error("ref not found", 404),
    };
    let page = build_blob_page(sql, owner, repo_name, ref_name, path, commit_hash)?;
    render_blob_html(&page, viewer, sql)
}

pub fn page_blob_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    ref_name: &str,
    path: &str,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let commit_hash = match api::resolve_ref(sql, ref_name)? {
        Some(hash) => hash,
        None => return Response::error("ref not found", 404),
    };
    let page = build_blob_page(sql, owner, repo_name, ref_name, path, commit_hash)?;
    presentation::markdown_response(&render_blob_markdown(&page, selection), selection)
}

fn render_breadcrumb(owner: &str, repo_name: &str, ref_name: &str, path: &str) -> String {
    let mut html = String::from(r#"<div class="breadcrumb">"#);
    html.push_str(&format!(
        r#"<a href="/{owner}/{repo}/">{repo}</a> / "#,
        owner = owner,
        repo = repo_name,
    ));

    if path.is_empty() {
        html.push_str("</div>");
        return html;
    }

    let parts: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    for (index, part) in parts.iter().enumerate() {
        if index < parts.len() - 1 {
            let sub_path = parts[..=index].join("/");
            html.push_str(&format!(
                r#"<a href="/{}/{}/tree/{}/{}">{}</a> / "#,
                owner,
                repo_name,
                ref_name,
                sub_path,
                html_escape(part),
            ));
        } else {
            html.push_str(&format!("<strong>{}</strong>", html_escape(part)));
        }
    }

    html.push_str("</div>");
    html
}

fn render_branch_selector(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    current_ref: &str,
    page_type: &str,
    path: &str,
) -> Result<String> {
    let branches = load_branches(sql)?;

    if branches.len() <= 1 {
        return Ok(format!(
            r#"<div class="branch-selector"><span class="branch-label">branch:</span> <strong>{}</strong></div>"#,
            html_escape(current_ref)
        ));
    }

    let mut html = String::from(
        r#"<div class="branch-selector"><span class="branch-label">branch:</span> <select onchange="window.location.href=this.value">"#,
    );

    for branch in &branches {
        let url = match page_type {
            "tree" => format!("/{}/{}/tree/{}/{}", owner, repo_name, branch, path),
            "blob" => format!("/{}/{}/blob/{}/{}", owner, repo_name, branch, path),
            "log" => format!("/{}/{}/commits?ref={}", owner, repo_name, branch),
            _ => format!("/{}/{}/?ref={}", owner, repo_name, branch),
        };
        let selected = if branch == current_ref {
            " selected"
        } else {
            ""
        };
        html.push_str(&format!(
            r#"<option value="{}"{}>{}</option>"#,
            url,
            selected,
            html_escape(branch)
        ));
    }

    html.push_str("</select></div>");
    Ok(html)
}

fn parent_path(path: &str) -> String {
    match path.rfind('/') {
        Some(position) => path[..position].to_string(),
        None => String::new(),
    }
}

fn lang_from_filename(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("");
    let lang = match ext {
        "rs" => "language-rust",
        "js" | "mjs" | "cjs" => "language-javascript",
        "ts" | "mts" | "cts" => "language-typescript",
        "py" => "language-python",
        "rb" => "language-ruby",
        "go" => "language-go",
        "java" => "language-java",
        "c" | "h" => "language-c",
        "cpp" | "cc" | "cxx" | "hpp" => "language-cpp",
        "cs" => "language-csharp",
        "swift" => "language-swift",
        "kt" | "kts" => "language-kotlin",
        "php" => "language-php",
        "sh" | "bash" | "zsh" => "language-bash",
        "json" => "language-json",
        "yaml" | "yml" => "language-yaml",
        "toml" => "language-toml",
        "xml" | "svg" | "html" | "htm" => "language-xml",
        "css" => "language-css",
        "scss" | "sass" => "language-scss",
        "sql" => "language-sql",
        "md" | "markdown" => "language-markdown",
        "dockerfile" | "Dockerfile" => "language-dockerfile",
        "makefile" | "Makefile" => "language-makefile",
        "zig" => "language-zig",
        "lua" => "language-lua",
        "r" | "R" => "language-r",
        "ex" | "exs" => "language-elixir",
        "erl" | "hrl" => "language-erlang",
        "hs" => "language-haskell",
        "ml" | "mli" => "language-ocaml",
        "nix" => "language-nix",
        _ => "",
    };
    lang.to_string()
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
