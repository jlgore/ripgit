//! Issues and pull requests: storage, retrieval, and merge logic.
//!
//! Routing is in lib.rs; HTML pages are in issues_web.rs.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use worker::*;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct IssueRow {
    pub id: i64,
    pub number: i64,
    pub kind: String, // "issue" | "pr"
    pub title: String,
    pub body: String,
    pub author_id: String,
    pub author_name: String,
    pub state: String, // "open" | "closed" | "merged"
    pub source_branch: Option<String>,
    pub target_branch: Option<String>,
    pub source_hash: Option<String>,
    pub merge_commit_hash: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[allow(dead_code)]
pub struct CommentRow {
    pub id: i64,
    pub issue_id: i64,
    pub author_id: String,
    pub author_name: String,
    pub body: String,
    pub created_at: i64,
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Map empty string to None. Used to round-trip optional fields through
/// DO SQLite which doesn't support SqlStorageValue::Null.
fn nonempty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.is_empty())
}

fn now_secs() -> i64 {
    worker::Date::now().as_millis() as i64 / 1000
}

fn next_number(sql: &SqlStorage) -> Result<i64> {
    #[derive(serde::Deserialize)]
    struct Row {
        n: i64,
    }
    let rows: Vec<Row> = sql
        .exec("SELECT COALESCE(MAX(number), 0) + 1 AS n FROM issues", None)?
        .to_array()?;
    Ok(rows.first().map(|r| r.n).unwrap_or(1))
}

fn resolve_ref_hash(sql: &SqlStorage, ref_name: &str) -> Result<Option<String>> {
    #[derive(serde::Deserialize)]
    struct Row {
        commit_hash: String,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT commit_hash FROM refs WHERE name = ? LIMIT 1",
            vec![SqlStorageValue::from(ref_name.to_string())],
        )?
        .to_array()?;
    Ok(rows.into_iter().next().map(|r| r.commit_hash))
}

fn get_commit_tree(sql: &SqlStorage, hash: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Row {
        tree_hash: String,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT tree_hash FROM commits WHERE hash = ? LIMIT 1",
            vec![SqlStorageValue::from(hash.to_string())],
        )?
        .to_array()?;
    rows.into_iter()
        .next()
        .map(|r| r.tree_hash)
        .ok_or_else(|| Error::RustError(format!("commit not found: {}", hash)))
}

fn get_parents(sql: &SqlStorage, hash: &str) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Row {
        parent_hash: String,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT parent_hash FROM commit_parents
             WHERE commit_hash = ? ORDER BY ordinal",
            vec![SqlStorageValue::from(hash.to_string())],
        )?
        .to_array()?;
    Ok(rows.into_iter().map(|r| r.parent_hash).collect())
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn create_issue(
    sql: &SqlStorage,
    kind: &str,
    title: &str,
    body: &str,
    author_id: &str,
    author_name: &str,
    source_branch: Option<&str>,
    target_branch: Option<&str>,
    source_hash: Option<&str>,
) -> Result<i64> {
    let number = next_number(sql)?;
    let now = now_secs();
    // DO SQLite's Rust bindings don't support SqlStorageValue::Null —
    // it throws "unrecognized JavaScript object". Use empty strings for
    // absent optional fields; map back to None on read.
    sql.exec(
        "INSERT INTO issues
            (number, kind, title, body, author_id, author_name, state,
             source_branch, target_branch, source_hash, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, 'open', ?, ?, ?, ?, ?)",
        vec![
            SqlStorageValue::from(number),
            SqlStorageValue::from(kind.to_string()),
            SqlStorageValue::from(title.to_string()),
            SqlStorageValue::from(body.to_string()),
            SqlStorageValue::from(author_id.to_string()),
            SqlStorageValue::from(author_name.to_string()),
            SqlStorageValue::from(source_branch.unwrap_or("").to_string()),
            SqlStorageValue::from(target_branch.unwrap_or("").to_string()),
            SqlStorageValue::from(source_hash.unwrap_or("").to_string()),
            SqlStorageValue::from(now),
            SqlStorageValue::from(now),
        ],
    )?;
    Ok(number)
}

pub fn get_issue(sql: &SqlStorage, number: i64) -> Result<Option<IssueRow>> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: i64,
        number: i64,
        kind: String,
        title: String,
        body: String,
        author_id: String,
        author_name: String,
        state: String,
        source_branch: Option<String>,
        target_branch: Option<String>,
        source_hash: Option<String>,
        merge_commit_hash: Option<String>,
        created_at: i64,
        updated_at: i64,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT id, number, kind, title, body, author_id, author_name, state,
                    source_branch, target_branch, source_hash, merge_commit_hash,
                    created_at, updated_at
             FROM issues WHERE number = ? LIMIT 1",
            vec![SqlStorageValue::from(number)],
        )?
        .to_array()?;
    Ok(rows.into_iter().next().map(|r| IssueRow {
        id: r.id,
        number: r.number,
        kind: r.kind,
        title: r.title,
        body: r.body,
        author_id: r.author_id,
        author_name: r.author_name,
        state: r.state,
        source_branch: nonempty(r.source_branch),
        target_branch: nonempty(r.target_branch),
        source_hash: nonempty(r.source_hash),
        merge_commit_hash: nonempty(r.merge_commit_hash),
        created_at: r.created_at,
        updated_at: r.updated_at,
    }))
}

pub fn list_issues(
    sql: &SqlStorage,
    kind: &str,
    state: &str,
    limit: usize,
    offset: usize,
) -> Result<Vec<IssueRow>> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: i64,
        number: i64,
        kind: String,
        title: String,
        body: String,
        author_id: String,
        author_name: String,
        state: String,
        source_branch: Option<String>,
        target_branch: Option<String>,
        source_hash: Option<String>,
        merge_commit_hash: Option<String>,
        created_at: i64,
        updated_at: i64,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT id, number, kind, title, body, author_id, author_name, state,
                    source_branch, target_branch, source_hash, merge_commit_hash,
                    created_at, updated_at
             FROM issues WHERE kind = ? AND state = ?
             ORDER BY number DESC LIMIT ? OFFSET ?",
            vec![
                SqlStorageValue::from(kind.to_string()),
                SqlStorageValue::from(state.to_string()),
                SqlStorageValue::from(limit as i64),
                SqlStorageValue::from(offset as i64),
            ],
        )?
        .to_array()?;
    Ok(rows
        .into_iter()
        .map(|r| IssueRow {
            id: r.id,
            number: r.number,
            kind: r.kind,
            title: r.title,
            body: r.body,
            author_id: r.author_id,
            author_name: r.author_name,
            state: r.state,
            source_branch: nonempty(r.source_branch),
            target_branch: nonempty(r.target_branch),
            source_hash: nonempty(r.source_hash),
            merge_commit_hash: nonempty(r.merge_commit_hash),
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect())
}

/// Count issues of a given kind by state ("open", "closed", "merged").
pub fn count_issues(sql: &SqlStorage, kind: &str, state: &str) -> Result<i64> {
    #[derive(serde::Deserialize)]
    struct Row {
        n: i64,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT COUNT(*) AS n FROM issues WHERE kind = ? AND state = ?",
            vec![
                SqlStorageValue::from(kind.to_string()),
                SqlStorageValue::from(state.to_string()),
            ],
        )?
        .to_array()?;
    Ok(rows.first().map(|r| r.n).unwrap_or(0))
}

/// Count issues that are NOT open (closed + merged) in a single query.
pub fn count_issues_not_open(sql: &SqlStorage, kind: &str) -> Result<i64> {
    #[derive(serde::Deserialize)]
    struct Row {
        n: i64,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT COUNT(*) AS n FROM issues WHERE kind = ? AND state != 'open'",
            vec![SqlStorageValue::from(kind.to_string())],
        )?
        .to_array()?;
    Ok(rows.first().map(|r| r.n).unwrap_or(0))
}

/// Close or reopen an issue. Only the issue author or the repo owner may act.
pub fn set_issue_state(
    sql: &SqlStorage,
    number: i64,
    new_state: &str,
    actor_name: &str,
    can_triage: bool,
) -> Result<()> {
    let issue = get_issue(sql, number)?
        .ok_or_else(|| Error::RustError(format!("issue #{} not found", number)))?;

    // Only allow closing to "closed"; reopen to "open". "merged" is set by merge_pr.
    if new_state != "open" && new_state != "closed" {
        return Err(Error::RustError("invalid state".into()));
    }

    if actor_name != issue.author_name && !can_triage {
        return Err(Error::RustError(
            "only the author or someone with triage access can change issue state".into(),
        ));
    }

    sql.exec(
        "UPDATE issues SET state = ?, updated_at = ? WHERE number = ?",
        vec![
            SqlStorageValue::from(new_state.to_string()),
            SqlStorageValue::from(now_secs()),
            SqlStorageValue::from(number),
        ],
    )?;
    Ok(())
}

pub fn create_comment(
    sql: &SqlStorage,
    issue_id: i64,
    body: &str,
    author_id: &str,
    author_name: &str,
) -> Result<i64> {
    let now = now_secs();
    sql.exec(
        "INSERT INTO issue_comments (issue_id, author_id, author_name, body, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        vec![
            SqlStorageValue::from(issue_id),
            SqlStorageValue::from(author_id.to_string()),
            SqlStorageValue::from(author_name.to_string()),
            SqlStorageValue::from(body.to_string()),
            SqlStorageValue::from(now),
            SqlStorageValue::from(now),
        ],
    )?;
    // Update parent issue's updated_at
    sql.exec(
        "UPDATE issues SET updated_at = ? WHERE id = ?",
        vec![SqlStorageValue::from(now), SqlStorageValue::from(issue_id)],
    )?;
    #[derive(serde::Deserialize)]
    struct LastId {
        id: i64,
    }
    let row: LastId = sql.exec("SELECT last_insert_rowid() AS id", None)?.one()?;
    Ok(row.id)
}

pub fn list_comments(sql: &SqlStorage, issue_id: i64) -> Result<Vec<CommentRow>> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: i64,
        issue_id: i64,
        author_id: String,
        author_name: String,
        body: String,
        created_at: i64,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT id, issue_id, author_id, author_name, body, created_at
             FROM issue_comments WHERE issue_id = ? ORDER BY id ASC",
            vec![SqlStorageValue::from(issue_id)],
        )?
        .to_array()?;
    Ok(rows
        .into_iter()
        .map(|r| CommentRow {
            id: r.id,
            issue_id: r.issue_id,
            author_id: r.author_id,
            author_name: r.author_name,
            body: r.body,
            created_at: r.created_at,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Pull request merge — three-way merge commit
// ---------------------------------------------------------------------------

/// Merge a PR by number. Creates a merge commit on the target branch.
/// Returns the hash of the new merge commit.
/// Only call this after verifying the actor is the repo owner.
pub fn merge_pr(sql: &SqlStorage, issue_number: i64, actor_name: &str) -> Result<String> {
    let issue = get_issue(sql, issue_number)?
        .ok_or_else(|| Error::RustError(format!("PR #{} not found", issue_number)))?;

    if issue.kind != "pr" {
        return Err(Error::RustError("not a pull request".into()));
    }
    if issue.state != "open" {
        return Err(Error::RustError(format!("PR is already {}", issue.state)));
    }

    let source_branch = issue
        .source_branch
        .as_deref()
        .ok_or_else(|| Error::RustError("PR missing source_branch".into()))?;
    let target_branch = issue
        .target_branch
        .as_deref()
        .ok_or_else(|| Error::RustError("PR missing target_branch".into()))?;

    let source_ref = format!("refs/heads/{}", source_branch);
    let target_ref = format!("refs/heads/{}", target_branch);

    let source_hash = resolve_ref_hash(sql, &source_ref)?
        .ok_or_else(|| Error::RustError(format!("branch not found: {}", source_branch)))?;
    let target_hash = resolve_ref_hash(sql, &target_ref)?
        .ok_or_else(|| Error::RustError(format!("branch not found: {}", target_branch)))?;

    if source_hash == target_hash {
        return Err(Error::RustError(
            "branches are identical, nothing to merge".into(),
        ));
    }

    let source_tree = get_commit_tree(sql, &source_hash)?;
    let target_tree = get_commit_tree(sql, &target_hash)?;

    // Find merge base for three-way merge
    let merged_tree = match find_merge_base(sql, &source_hash, &target_hash)? {
        Some(base) if base == target_hash => {
            // Target is an ancestor of source: fast-forward.
            // Use source tree directly — no conflict possible.
            source_tree.clone()
        }
        Some(base) => {
            // True diverged merge: three-way tree merge
            let base_tree = get_commit_tree(sql, &base)?;
            let base_files = flatten_tree(sql, &base_tree, "")?;
            let target_files = flatten_tree(sql, &target_tree, "")?;
            let source_files = flatten_tree(sql, &source_tree, "")?;

            let merged_files = merge_three_way(&base_files, &target_files, &source_files)
                .map_err(Error::RustError)?;

            build_tree_from_files(sql, "", &merged_files)?
        }
        None => {
            // No common history — use source tree as merged result
            source_tree.clone()
        }
    };

    // Build and store the merge commit
    let now = now_secs();
    let actor_email = format!("{}@noreply", actor_name);
    let message = format!(
        "Merge branch '{}' into '{}'\n\nMerge PR #{}: {}",
        source_branch, target_branch, issue_number, issue.title
    );

    let commit_content = serialize_commit_content(
        &merged_tree,
        &[target_hash.as_str(), source_hash.as_str()],
        actor_name,
        &actor_email,
        now,
        &message,
    );

    let merge_commit_hash = git_sha1("commit", &commit_content);

    let parsed = crate::store::ParsedCommit {
        tree_hash: merged_tree,
        parents: vec![target_hash.clone(), source_hash.clone()],
        author: actor_name.to_string(),
        author_email: actor_email.clone(),
        author_time: now,
        committer: actor_name.to_string(),
        committer_email: actor_email,
        commit_time: now,
        message,
    };
    crate::store::store_commit(sql, &merge_commit_hash, &parsed, &commit_content, false)?;

    // Advance target branch to merge commit
    sql.exec(
        "INSERT INTO refs (name, commit_hash) VALUES (?, ?)
         ON CONFLICT(name) DO UPDATE SET commit_hash = ?",
        vec![
            SqlStorageValue::from(target_ref),
            SqlStorageValue::from(merge_commit_hash.clone()),
            SqlStorageValue::from(merge_commit_hash.clone()),
        ],
    )?;

    // Mark the PR as merged
    sql.exec(
        "UPDATE issues SET state = 'merged', merge_commit_hash = ?, updated_at = ?
         WHERE number = ?",
        vec![
            SqlStorageValue::from(merge_commit_hash.clone()),
            SqlStorageValue::from(now),
            SqlStorageValue::from(issue_number),
        ],
    )?;

    Ok(merge_commit_hash)
}

// ---------------------------------------------------------------------------
// Merge helpers
// ---------------------------------------------------------------------------

/// BFS-based merge base (lowest common ancestor of two commits).
pub(crate) fn find_merge_base(sql: &SqlStorage, a: &str, b: &str) -> Result<Option<String>> {
    if a == b {
        return Ok(Some(a.to_string()));
    }

    let mut visited_a: HashSet<String> = HashSet::new();
    let mut visited_b: HashSet<String> = HashSet::new();
    let mut queue_a: VecDeque<String> = VecDeque::new();
    let mut queue_b: VecDeque<String> = VecDeque::new();

    visited_a.insert(a.to_string());
    visited_b.insert(b.to_string());
    queue_a.push_back(a.to_string());
    queue_b.push_back(b.to_string());

    while !queue_a.is_empty() || !queue_b.is_empty() {
        if let Some(current) = queue_a.pop_front() {
            for parent in get_parents(sql, &current)? {
                if visited_b.contains(&parent) {
                    return Ok(Some(parent));
                }
                if visited_a.insert(parent.clone()) {
                    queue_a.push_back(parent);
                }
            }
        }
        if let Some(current) = queue_b.pop_front() {
            for parent in get_parents(sql, &current)? {
                if visited_a.contains(&parent) {
                    return Ok(Some(parent));
                }
                if visited_b.insert(parent.clone()) {
                    queue_b.push_back(parent);
                }
            }
        }
    }

    Ok(None)
}

/// Recursively flatten a git tree into a map: path → (mode, blob_hash).
/// Only leaf entries (non-tree) are included.
fn flatten_tree(
    sql: &SqlStorage,
    tree_hash: &str,
    prefix: &str,
) -> Result<HashMap<String, (u32, String)>> {
    #[derive(serde::Deserialize)]
    struct Row {
        name: String,
        mode: i64,
        entry_hash: String,
    }
    let rows: Vec<Row> = sql
        .exec(
            "SELECT name, mode, entry_hash FROM trees WHERE tree_hash = ?",
            vec![SqlStorageValue::from(tree_hash.to_string())],
        )?
        .to_array()?;

    let mut files = HashMap::new();
    for row in rows {
        let path = if prefix.is_empty() {
            row.name.clone()
        } else {
            format!("{}/{}", prefix, row.name)
        };
        if row.mode == 0o040000 {
            let subtree = flatten_tree(sql, &row.entry_hash, &path)?;
            files.extend(subtree);
        } else {
            files.insert(path, (row.mode as u32, row.entry_hash));
        }
    }
    Ok(files)
}

/// Three-way file-level merge. Returns a conflict error listing conflicting paths.
fn merge_three_way(
    base_files: &HashMap<String, (u32, String)>,
    target_files: &HashMap<String, (u32, String)>,
    source_files: &HashMap<String, (u32, String)>,
) -> std::result::Result<HashMap<String, (u32, String)>, String> {
    let mut merged = target_files.clone();
    let mut conflicts: Vec<String> = Vec::new();

    // Apply source changes
    for (path, (s_mode, s_hash)) in source_files {
        let base_hash = base_files.get(path).map(|(_, h)| h.as_str());
        let target = target_files.get(path);

        // If source is identical to base, no change from source side
        if base_hash == Some(s_hash.as_str()) {
            continue;
        }

        match target {
            None => {
                if base_hash.is_none() {
                    // New in source, not in target → add
                    merged.insert(path.clone(), (*s_mode, s_hash.clone()));
                } else {
                    // Target deleted it; source modified → conflict
                    conflicts.push(path.clone());
                }
            }
            Some((_, t_hash)) => {
                let target_changed = base_hash != Some(t_hash.as_str());
                if !target_changed {
                    // Target unchanged → apply source change
                    merged.insert(path.clone(), (*s_mode, s_hash.clone()));
                } else if t_hash == s_hash {
                    // Both changed to same content → fine
                } else {
                    // Both changed differently → conflict
                    conflicts.push(path.clone());
                }
            }
        }
    }

    // Apply source deletions
    for (path, (_, b_hash)) in base_files {
        if source_files.contains_key(path) {
            continue; // not deleted in source
        }
        if let Some((_, t_hash)) = target_files.get(path) {
            if t_hash == b_hash {
                // Target unchanged → apply deletion
                merged.remove(path);
            } else {
                // Target modified it; source deleted → conflict
                conflicts.push(path.clone());
            }
        }
        // If target already deleted it, nothing to do
    }

    if !conflicts.is_empty() {
        conflicts.sort();
        return Err(format!("merge conflict in: {}", conflicts.join(", ")));
    }

    Ok(merged)
}

/// Build git tree objects bottom-up from a flat file map.
/// Returns the hash of the root tree.
fn build_tree_from_files(
    sql: &SqlStorage,
    dir: &str,
    all_files: &HashMap<String, (u32, String)>,
) -> Result<String> {
    let dir_prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{}/", dir)
    };

    let mut entries: BTreeMap<String, (u32, String)> = BTreeMap::new();
    let mut seen_subdirs: HashSet<String> = HashSet::new();

    for (path, (mode, hash)) in all_files {
        let rel = if dir.is_empty() {
            path.as_str()
        } else if let Some(r) = path.strip_prefix(&dir_prefix) {
            r
        } else {
            continue;
        };

        if let Some(slash_pos) = rel.find('/') {
            let subdir_name = &rel[..slash_pos];
            if seen_subdirs.insert(subdir_name.to_string()) {
                let full_subdir = if dir.is_empty() {
                    subdir_name.to_string()
                } else {
                    format!("{}/{}", dir, subdir_name)
                };
                let subtree_hash = build_tree_from_files(sql, &full_subdir, all_files)?;
                entries.insert(subdir_name.to_string(), (0o040000u32, subtree_hash));
            }
        } else {
            entries.insert(rel.to_string(), (*mode, hash.clone()));
        }
    }

    let content = serialize_tree_content(&entries);
    let tree_hash = git_sha1("tree", &content);
    crate::store::store_tree(sql, &tree_hash, &content)?;
    Ok(tree_hash)
}

/// Serialize a directory's entries into git tree binary format (no header).
/// Git sort order: treat directory names as if they end with '/'.
fn serialize_tree_content(entries: &BTreeMap<String, (u32, String)>) -> Vec<u8> {
    let mut sorted: Vec<(&String, &(u32, String))> = entries.iter().collect();
    sorted.sort_by(|(a_name, (a_mode, _)), (b_name, (b_mode, _))| {
        let a_key = if *a_mode == 0o040000 {
            format!("{}/", a_name)
        } else {
            (*a_name).clone()
        };
        let b_key = if *b_mode == 0o040000 {
            format!("{}/", b_name)
        } else {
            (*b_name).clone()
        };
        a_key.cmp(&b_key)
    });

    let mut buf = Vec::new();
    for (name, (mode, hash)) in &sorted {
        write_tree_entry(&mut buf, *mode, name, hash);
    }
    buf
}

fn write_tree_entry(buf: &mut Vec<u8>, mode: u32, name: &str, hash: &str) {
    let mode_str = format!("{:o}", mode);
    buf.extend_from_slice(mode_str.as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    if let Ok(bytes) = hex_to_bytes(hash) {
        buf.extend_from_slice(&bytes);
    }
}

/// SHA-1 of a git object: "type size\0content"
fn git_sha1(obj_type: &str, content: &[u8]) -> String {
    let header = format!("{} {}\0", obj_type, content.len());
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(content);
    hasher.digest().to_string()
}

fn hex_to_bytes(hex: &str) -> std::result::Result<[u8; 20], ()> {
    if hex.len() != 40 {
        return Err(());
    }
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(out)
}

/// Build git commit content (no "commit N\0" header — that's only for hashing).
fn serialize_commit_content(
    tree_hash: &str,
    parent_hashes: &[&str],
    author_name: &str,
    author_email: &str,
    timestamp: i64,
    message: &str,
) -> Vec<u8> {
    let mut s = format!("tree {}\n", tree_hash);
    for p in parent_hashes {
        s.push_str(&format!("parent {}\n", p));
    }
    let ident = format!("{} <{}> {} +0000", author_name, author_email, timestamp);
    s.push_str(&format!("author {}\n", ident));
    s.push_str(&format!("committer {}\n", ident));
    s.push('\n');
    s.push_str(message);
    s.into_bytes()
}

// ---------------------------------------------------------------------------
// Form parsing utility
// ---------------------------------------------------------------------------

/// Decode a URL-encoded form body into a key→value map.
pub fn parse_form(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in body.split('&') {
        let mut kv = pair.splitn(2, '=');
        if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
            map.insert(percent_decode(k), percent_decode(v));
        }
    }
    map
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
