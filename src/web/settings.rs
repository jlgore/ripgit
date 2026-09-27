use super::*;
use crate::authz::RepoAccessInfo;

struct SettingsPage<'a> {
    owner: String,
    repo_name: String,
    commits: i64,
    blobs: i64,
    db_bytes: u64,
    default_branch: String,
    /// "public" | "internal" | "private"
    visibility: &'a str,
    access: &'a RepoAccessInfo,
}

const ROLES: [&str; 4] = ["read", "triage", "write", "admin"];

impl SettingsPage<'_> {
    fn settings_path(&self) -> String {
        format!("/{}/{}/settings", self.owner, self.repo_name)
    }

    fn action_path(&self, sub: &str) -> String {
        format!("{}/{}", self.settings_path(), sub)
    }

    fn db_mb(&self) -> f64 {
        self.db_bytes as f64 / 1_048_576.0
    }
}

fn build_settings_page<'a>(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    visibility: &'a str,
    access: &'a RepoAccessInfo,
) -> Result<SettingsPage<'a>> {
    #[derive(serde::Deserialize)]
    struct CountRow {
        n: i64,
    }

    let commits = sql
        .exec("SELECT COUNT(*) AS n FROM commits", None)?
        .to_array::<CountRow>()?
        .first()
        .map(|row| row.n)
        .unwrap_or(0);
    let blobs = sql
        .exec("SELECT COUNT(*) AS n FROM blobs", None)?
        .to_array::<CountRow>()?
        .first()
        .map(|row| row.n)
        .unwrap_or(0);

    Ok(SettingsPage {
        owner: owner.to_string(),
        repo_name: repo_name.to_string(),
        commits,
        blobs,
        db_bytes: sql.database_size() as u64,
        default_branch: store::get_config(sql, "default_branch")?
            .unwrap_or_else(|| "refs/heads/main".to_string()),
        visibility,
        access,
    })
}

fn render_settings_html(page: &SettingsPage, viewer: Viewer<'_>) -> String {
    let mut html = String::new();
    html.push_str(&format!(
        r#"
<section class="settings-section">
  <h2>Repository stats</h2>
  <div class="stats-grid">
    <div class="stat-box"><div class="stat-val">{commits}</div><div class="stat-lbl">commits</div></div>
    <div class="stat-box"><div class="stat-val">{blobs}</div><div class="stat-lbl">blobs</div></div>
    <div class="stat-box"><div class="stat-val">{db_mb:.1} MB</div><div class="stat-lbl">database size</div></div>
  </div>
</section>"#,
        commits = page.commits,
        blobs = page.blobs,
        db_mb = page.db_mb(),
    ));

    html.push_str(&format!(
        r#"
<section class="settings-section">
  <h2>Search indexes</h2>
  <p class="settings-hint">Rebuild after a bulk push or if search results look stale.</p>
  <div class="action-row">
    <form method="POST" action="{commit_graph}">
      <button class="btn-action" type="submit">Rebuild commit graph</button>
      <span class="action-hint">Required for commit history and log</span>
    </form>
    <form method="POST" action="{fts_commits}">
      <button class="btn-action" type="submit">Rebuild commit search</button>
      <span class="action-hint">Full-text search over commit messages</span>
    </form>
    <form method="POST" action="{fts_head}">
      <button class="btn-action" type="submit">Rebuild code search</button>
      <span class="action-hint">Full-text search over file contents (slow on large repos)</span>
    </form>
  </div>
</section>"#,
        commit_graph = page.action_path("rebuild-graph"),
        fts_commits = page.action_path("rebuild-fts-commits"),
        fts_head = page.action_path("rebuild-fts"),
    ));

    html.push_str(&render_access_html(page));

    html.push_str(&format!(
        r#"
<section class="settings-section">
  <h2>Default branch</h2>
  <form method="POST" action="{action}" class="inline-form">
    <input type="text" name="branch" value="{branch}" class="branch-input" placeholder="refs/heads/main">
    <button class="btn-action" type="submit">Save</button>
  </form>
</section>"#,
        action = page.action_path("default-branch"),
        branch = html_escape(&page.default_branch),
    ));

    html.push_str(&format!(
        r#"
<section class="settings-section settings-danger">
  <h2>Danger zone</h2>
  <p class="settings-hint">This will permanently delete all data for <strong>{owner}/{repo}</strong>. There is no undo.</p>
  <form method="POST" action="{action}" class="inline-form">
    <input type="text" name="confirm" placeholder='Type "{owner}/{repo}" to confirm' class="branch-input danger-confirm">
    <button class="btn-danger-action" type="submit">Delete repository</button>
  </form>
</section>"#,
        owner = html_escape(&page.owner),
        repo = html_escape(&page.repo_name),
        action = page.action_path("delete"),
    ));

    layout(
        "Settings",
        &page.owner,
        &page.repo_name,
        &page.default_branch,
        viewer,
        &html,
    )
}

fn visibility_options(page: &SettingsPage) -> Vec<(&'static str, &'static str)> {
    let mut options = vec![("public", "Public — anyone can read")];
    if page.access.is_org {
        options.push(("internal", "Internal — members of the organization can read"));
    }
    options.push(("private", "Private — only people and teams granted access"));
    options
}

fn role_select(name: &str) -> String {
    let options: String = ROLES
        .iter()
        .map(|r| format!(r#"<option value="{r}"{sel}>{r}</option>"#, sel = if *r == "read" { " selected" } else { "" }))
        .collect();
    format!(r#"<select name="{name}" class="branch-input">{options}</select>"#)
}

fn render_access_html(page: &SettingsPage) -> String {
    let visibility_options: String = visibility_options(page)
        .into_iter()
        .map(|(value, label)| {
            format!(
                r#"<option value="{value}"{sel}>{label}</option>"#,
                sel = if value == page.visibility { " selected" } else { "" },
            )
        })
        .collect();

    let grants = if page.access.grants.is_empty() {
        r#"<p class="settings-hint">No one has been granted access individually.</p>"#.to_string()
    } else {
        let rows: String = page
            .access
            .grants
            .iter()
            .map(|g| {
                format!(
                    r#"<tr><td>{kind}</td><td>{label}</td><td>{role}</td><td>
  <form method="POST" action="{action}" class="inline-form">
    <input type="hidden" name="kind" value="{kind}">
    <input type="hidden" name="id" value="{id}">
    <button class="btn-action" type="submit">Remove</button>
  </form></td></tr>"#,
                    kind = html_escape(&g.kind),
                    label = html_escape(&g.label),
                    role = html_escape(&g.role),
                    id = html_escape(&g.id),
                    action = page.action_path("revoke"),
                )
            })
            .collect();
        format!(r#"<table class="grants"><thead><tr><th>Kind</th><th>Who</th><th>Role</th><th></th></tr></thead><tbody>{rows}</tbody></table>"#)
    };

    let team_form = if page.access.is_org && !page.access.teams.is_empty() {
        let teams: String = page
            .access
            .teams
            .iter()
            .map(|t| format!(r#"<option value="{}">{}</option>"#, html_escape(&t.id), html_escape(&t.name)))
            .collect();
        format!(
            r#"
  <form method="POST" action="{action}" class="inline-form">
    <input type="hidden" name="kind" value="team">
    <select name="grantee" class="branch-input">{teams}</select>
    {roles}
    <button class="btn-action" type="submit">Grant to team</button>
  </form>"#,
            action = page.action_path("grant"),
            roles = role_select("role"),
        )
    } else {
        String::new()
    };

    format!(
        r#"
<section class="settings-section">
  <h2>Visibility</h2>
  <form method="POST" action="{visibility_action}" class="inline-form">
    <select name="visibility" class="branch-input">{visibility_options}</select>
    <button class="btn-action" type="submit">Save</button>
  </form>
</section>
<section class="settings-section">
  <h2>Access</h2>
  <p class="settings-hint">Roles: read &lt; triage (close issues) &lt; write (push, merge) &lt; admin (these settings).</p>
  {grants}
  <form method="POST" action="{grant_action}" class="inline-form">
    <input type="hidden" name="kind" value="user">
    <input type="text" name="grantee" placeholder="GitHub login" class="branch-input" required>
    {roles}
    <button class="btn-action" type="submit">Grant to user</button>
  </form>{team_form}
</section>"#,
        visibility_action = page.action_path("visibility"),
        grant_action = page.action_path("grant"),
        roles = role_select("role"),
    )
}

fn render_settings_markdown(page: &SettingsPage, selection: &NegotiatedRepresentation) -> String {
    let mut markdown = format!(
        "# {}/{} settings\n\nAdmin-only repository maintenance page.\n\n## Repository Stats\n- Commits: `{}`\n- Blobs: `{}`\n- Database size: `{:.1} MB` (`{}` bytes)\n\n## Current Configuration\n- Settings page: `{}`\n- Default branch: `{}`\n- Code search rebuilds index the current default branch only.\n",
        page.owner,
        page.repo_name,
        page.commits,
        page.blobs,
        page.db_mb(),
        page.db_bytes,
        page.settings_path(),
        page.default_branch,
    );

    markdown.push_str(&format!("\n## Access\n- Visibility: `{}`\n", page.visibility));
    if page.access.grants.is_empty() {
        markdown.push_str("- Grants: none\n");
    } else {
        for g in &page.access.grants {
            markdown.push_str(&format!(
                "- {} `{}` (id `{}`): `{}`\n",
                g.kind, g.label, g.id, g.role
            ));
        }
    }
    for t in &page.access.teams {
        markdown.push_str(&format!("- Team available to grant: `{}` (id `{}`)\n", t.name, t.id));
    }

    let visibility_values: Vec<&str> = visibility_options(page).into_iter().map(|(v, _)| v).collect();
    let mut actions = vec![
        Action::post(
            page.action_path("visibility"),
            "set who may read this repository without a grant",
        )
        .with_requires("repo admin")
        .with_fields(vec![presentation::ActionField::required(
            "visibility",
            &format!("one of `{}`", visibility_values.join("`, `")),
        )])
        .with_effect("stores the visibility, updates the owner profile listing, then redirects back to settings"),
        Action::post(
            page.action_path("grant"),
            "grant a user or team a role on this repository",
        )
        .with_requires("repo admin")
        .with_fields(vec![
            presentation::ActionField::required("kind", "`user` or `team`"),
            presentation::ActionField::required(
                "grantee",
                "a user's GitHub login (they must have signed in once), or a team id from this page",
            ),
            presentation::ActionField::required("role", "`read`, `triage`, `write`, or `admin`"),
        ])
        .with_effect("adds the grant or replaces the grantee's existing role, then redirects back to settings; unknown users or teams return `400`"),
        Action::post(
            page.action_path("revoke"),
            "remove a grant",
        )
        .with_requires("repo admin")
        .with_fields(vec![
            presentation::ActionField::required("kind", "`user` or `team`"),
            presentation::ActionField::required("id", "the grantee id listed above"),
        ])
        .with_effect("deletes the grant, then redirects back to settings"),
    ];
    actions.extend(vec![
        Action::post(
            page.action_path("rebuild-graph"),
            "rebuild the commit ancestry graph used by history and log traversal",
        )
        .with_requires("repo admin")
        .with_effect("deletes existing `commit_graph` rows, regenerates them from `commit_parents`, then redirects back to settings"),
        Action::post(
            page.action_path("rebuild-fts-commits"),
            "rebuild the commit search index over commit messages and authors",
        )
        .with_requires("repo admin")
        .with_effect("clears `fts_commits`, re-inserts every commit, then redirects back to settings"),
        Action::post(
            page.action_path("rebuild-fts"),
            "rebuild the code search index for the saved default branch",
        )
        .with_requires("repo admin")
        .with_effect("looks up the current `default_branch`, rebuilds the HEAD file-content index from that ref when it exists, then redirects back to settings"),
        Action::post(
            page.action_path("default-branch"),
            "save the repository default branch used by the UI and code-search rebuilds",
        )
        .with_requires("repo admin")
        .with_fields(vec![presentation::ActionField::required(
            "branch",
            "full ref name to store, for example `refs/heads/main`; empty or whitespace-only input leaves the current value unchanged",
        )])
        .with_effect("stores `default_branch` exactly as submitted after trimming outer whitespace, then redirects back to settings"),
        Action::post(
            page.action_path("delete"),
            "permanently delete this repository",
        )
        .with_requires("repo admin")
        .with_fields(vec![presentation::ActionField::required(
            "confirm",
            &format!(
                "must exactly match `{}/{}` to proceed",
                page.owner, page.repo_name
            ),
        )])
        .with_effect(&format!(
            "danger: on an exact match, deletes all Durable Object storage for `{}/{}` and redirects to `/{}/`; any other value leaves the repository intact and redirects back to settings",
            page.owner, page.repo_name, page.owner
        )),
    ]);

    let hints = vec![
        presentation::text_navigation_hint(*selection),
        Hint::new("All settings mutations here are POST-only and require the admin role on this repository."),
        Hint::new("Use fully qualified refs like `refs/heads/main` for the default branch; this form does not verify that the ref exists before saving."),
        Hint::new("Danger: repository deletion is irreversible because it clears the repository Durable Object storage."),
    ];

    markdown.push_str(&presentation::render_actions_section(&actions));
    markdown.push_str(&presentation::render_hints_section(&hints));
    markdown
}

pub fn page_settings(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    viewer: Viewer<'_>,
    visibility: &str,
    access: &RepoAccessInfo,
) -> Result<Response> {
    let page = build_settings_page(sql, owner, repo_name, visibility, access)?;
    html_response(&render_settings_html(&page, viewer))
}

pub fn page_settings_markdown(
    sql: &SqlStorage,
    owner: &str,
    repo_name: &str,
    visibility: &str,
    access: &RepoAccessInfo,
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let page = build_settings_page(sql, owner, repo_name, visibility, access)?;
    presentation::markdown_response(&render_settings_markdown(&page, selection), selection)
}
