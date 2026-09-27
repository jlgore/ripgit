//! CI: pipeline discovery, runs and their status, and the source archive
//! handed to runners.
//!
//! Pipelines are TypeScript files in `.ripgit/pipelines/`. A push that moves a
//! branch creates one run per pipeline file at the new commit and hands it to
//! the `ripgit-ci` Worker (service binding `CI`), which plans it, runs its
//! jobs in sandboxes, and reports progress back to `POST ci/report`. See
//! docs/spec-orgs-and-ci.md, Part 2.
//!
//! Runs are numbered per repo, like issues; jobs and steps are keyed by that
//! number (`ci_jobs.run_id` / `ci_steps.run_id` hold the run number).

use crate::store;
use serde::{Deserialize, Serialize};
use worker::*;

/// Where pipeline files live in a repository.
pub const PIPELINES_DIR: &str = ".ripgit/pipelines";

const MODE_TREE: u32 = 0o040000;
const MODE_SYMLINK: u32 = 0o120000;
const MODE_SUBMODULE: u32 = 0o160000;
const MODE_EXECUTABLE: u32 = 0o100755;

// ---------------------------------------------------------------------------
// Source archive (ustar, PAX for long names)
// ---------------------------------------------------------------------------

struct TarWriter {
    out: Vec<u8>,
    mtime: u64,
}

impl TarWriter {
    fn new(mtime: u64) -> Self {
        TarWriter {
            out: Vec::new(),
            mtime,
        }
    }

    /// Write `value` into `field` as zero-padded octal followed by a NUL.
    fn octal(field: &mut [u8], value: u64) {
        let digits = field.len() - 1;
        let text = format!("{:0width$o}", value, width = digits);
        field[..digits].copy_from_slice(&text.as_bytes()[text.len() - digits..]);
        field[digits] = 0;
    }

    fn header(&self, name: &str, mode: u32, size: u64, typeflag: u8, link: &str) -> [u8; 512] {
        let mut h = [0u8; 512];
        let clip = |s: &str, n: usize| -> Vec<u8> { s.as_bytes().iter().take(n).copied().collect() };
        let name = clip(name, 100);
        h[..name.len()].copy_from_slice(&name);
        Self::octal(&mut h[100..108], mode as u64);
        Self::octal(&mut h[108..116], 0); // uid
        Self::octal(&mut h[116..124], 0); // gid
        Self::octal(&mut h[124..136], size);
        Self::octal(&mut h[136..148], self.mtime);
        h[156] = typeflag;
        let link = clip(link, 100);
        h[157..157 + link.len()].copy_from_slice(&link);
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        // Checksum: sum of all header bytes with the checksum field as spaces.
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        let text = format!("{:06o}\0 ", sum);
        h[148..156].copy_from_slice(text.as_bytes());
        h
    }

    fn write_data(&mut self, data: &[u8]) {
        self.out.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        self.out.extend(std::iter::repeat(0u8).take(pad));
    }

    /// One PAX record: "<len> <key>=<value>\n", where len counts itself.
    fn pax_record(key: &str, value: &str) -> String {
        let base = key.len() + value.len() + 3;
        let mut len = base + 1;
        while len != base + len.to_string().len() {
            len = base + len.to_string().len();
        }
        format!("{} {}={}\n", len, key, value)
    }

    fn append(&mut self, path: &str, mode: u32, typeflag: u8, data: &[u8], link: &str) {
        let mut pax = String::new();
        if path.len() > 100 {
            pax.push_str(&Self::pax_record("path", path));
        }
        if link.len() > 100 {
            pax.push_str(&Self::pax_record("linkpath", link));
        }
        if !pax.is_empty() {
            let header = self.header("././@PaxHeader", 0o644, pax.len() as u64, b'x', "");
            self.out.extend_from_slice(&header);
            self.write_data(pax.as_bytes());
        }
        let header = self.header(path, mode, data.len() as u64, typeflag, link);
        self.out.extend_from_slice(&header);
        self.write_data(data);
    }

    fn finish(mut self) -> Vec<u8> {
        self.out.extend(std::iter::repeat(0u8).take(1024));
        self.out
    }
}

fn walk_tree(sql: &SqlStorage, tree_hash: &str, prefix: &str, tar: &mut TarWriter) -> Result<()> {
    let mut entries = store::load_tree_from_db(sql, tree_hash)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{}/{}", prefix, entry.name)
        };
        match entry.mode {
            MODE_TREE => walk_tree(sql, &entry.hash, &path, tar)?,
            // A submodule is another repository. Like `git archive`, leave its
            // directory in place, empty.
            MODE_SUBMODULE => tar.append(&format!("{}/", path), 0o755, b'5', &[], ""),
            MODE_SYMLINK => {
                let target = store::reconstruct_blob_by_hash(sql, &entry.hash)?.unwrap_or_default();
                tar.append(&path, 0o777, b'2', &[], &String::from_utf8_lossy(&target));
            }
            mode => {
                let data = store::reconstruct_blob_by_hash(sql, &entry.hash)?
                    .ok_or_else(|| Error::RustError(format!("blob {} missing", entry.hash)))?;
                let perm = if mode == MODE_EXECUTABLE { 0o755 } else { 0o644 };
                tar.append(&path, perm, b'0', &data, "");
            }
        }
    }
    Ok(())
}

/// The tree at `sha` as a tar archive, like `git archive --format=tar`
/// (without the global commit-id header). None if the commit is unknown.
///
/// Buffered in memory: bounded by the repo's working tree, which the 50 MB
/// pack limit keeps well under the DO's 128 MB in practice.
pub fn archive(sql: &SqlStorage, sha: &str) -> Result<Option<Vec<u8>>> {
    #[derive(Deserialize)]
    struct Row {
        commit_time: i64,
    }
    let Some(tree) = store::commit_tree_hash(sql, sha)? else {
        return Ok(None);
    };
    let mtime = sql
        .exec(
            "SELECT commit_time FROM commits WHERE hash = ?",
            vec![SqlStorageValue::from(sha.to_string())],
        )?
        .to_array::<Row>()?
        .first()
        .map(|r| r.commit_time.max(0) as u64)
        .unwrap_or(0);
    let mut tar = TarWriter::new(mtime);
    walk_tree(sql, &tree, "", &mut tar)?;
    Ok(Some(tar.finish()))
}

/// Pipeline files (`.ripgit/pipelines/*.ts`) in the tree at `sha`.
pub fn pipelines_at(sql: &SqlStorage, sha: &str) -> Result<Vec<String>> {
    let Some(mut tree) = store::commit_tree_hash(sql, sha)? else {
        return Ok(vec![]);
    };
    for dir in PIPELINES_DIR.split('/') {
        let entries = store::load_tree_from_db(sql, &tree)?;
        match entries.into_iter().find(|e| e.name == dir && e.mode == MODE_TREE) {
            Some(e) => tree = e.hash,
            None => return Ok(vec![]),
        }
    }
    let mut files: Vec<String> = store::load_tree_from_db(sql, &tree)?
        .into_iter()
        .filter(|e| e.name.ends_with(".ts") && e.mode != MODE_TREE && e.mode != MODE_SUBMODULE)
        .map(|e| format!("{}/{}", PIPELINES_DIR, e.name))
        .collect();
    files.sort();
    Ok(files)
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

/// Statuses a run, job, or step can report.
const STATUSES: &[&str] = &[
    "planning", "queued", "running", "success", "failure", "skipped", "error", "cancelled",
];

fn is_terminal(status: &str) -> bool {
    matches!(status, "success" | "failure" | "skipped" | "error" | "cancelled")
}

fn now_secs() -> i64 {
    (Date::now().as_millis() / 1000) as i64
}

fn v<T: Into<SqlStorageValue>>(x: T) -> SqlStorageValue {
    x.into()
}

#[derive(Deserialize, Serialize, Clone)]
pub struct RunRow {
    pub number: i64,
    pub pipeline: String,
    pub name: String,
    pub event: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    pub status: String,
    pub actor: String,
    pub error: String,
    pub created_at: i64,
    pub finished_at: i64,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct JobRow {
    pub name: String,
    pub status: String,
    pub started_at: i64,
    pub finished_at: i64,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct StepRow {
    pub job: String,
    pub idx: i64,
    pub name: String,
    pub status: String,
    pub exit_code: i64,
    pub log_key: String,
    pub started_at: i64,
    pub finished_at: i64,
}

const RUN_COLUMNS: &str =
    "number, pipeline, name, event, ref, sha, status, actor, error, created_at, finished_at";

/// Record a new run in `planning` and return its number.
pub fn create_run(
    sql: &SqlStorage,
    pipeline: &str,
    event: &str,
    ref_name: &str,
    sha: &str,
    actor: &str,
) -> Result<i64> {
    #[derive(Deserialize)]
    struct Next {
        n: i64,
    }
    let number = sql
        .exec("SELECT COALESCE(MAX(number), 0) + 1 AS n FROM ci_runs", None)?
        .to_array::<Next>()?
        .first()
        .map(|r| r.n)
        .unwrap_or(1);
    sql.exec(
        "INSERT INTO ci_runs (number, pipeline, event, ref, sha, status, actor, created_at)
         VALUES (?, ?, ?, ?, ?, 'planning', ?, ?)",
        vec![
            v(number as f64),
            v(pipeline),
            v(event),
            v(ref_name),
            v(sha),
            v(actor),
            v(now_secs() as f64),
        ],
    )?;
    Ok(number)
}

pub fn fail_run(sql: &SqlStorage, number: i64, message: &str) -> Result<()> {
    sql.exec(
        "UPDATE ci_runs SET status = 'error', error = ?, finished_at = ? WHERE number = ?",
        vec![v(message), v(now_secs() as f64), v(number as f64)],
    )?;
    Ok(())
}

pub fn list_runs(sql: &SqlStorage, limit: i64) -> Result<Vec<RunRow>> {
    sql.exec(
        &format!("SELECT {} FROM ci_runs ORDER BY number DESC LIMIT ?", RUN_COLUMNS),
        vec![v(limit as f64)],
    )?
    .to_array()
}

pub fn get_run(sql: &SqlStorage, number: i64) -> Result<Option<RunRow>> {
    Ok(sql
        .exec(
            &format!("SELECT {} FROM ci_runs WHERE number = ?", RUN_COLUMNS),
            vec![v(number as f64)],
        )?
        .to_array::<RunRow>()?
        .into_iter()
        .next())
}

pub fn jobs(sql: &SqlStorage, number: i64) -> Result<Vec<JobRow>> {
    sql.exec(
        "SELECT name, status, started_at, finished_at FROM ci_jobs WHERE run_id = ? ORDER BY rowid",
        vec![v(number as f64)],
    )?
    .to_array()
}

pub fn steps(sql: &SqlStorage, number: i64) -> Result<Vec<StepRow>> {
    sql.exec(
        "SELECT job, idx, name, status, exit_code, log_key, started_at, finished_at
         FROM ci_steps WHERE run_id = ? ORDER BY job, idx",
        vec![v(number as f64)],
    )?
    .to_array()
}

// ---------------------------------------------------------------------------
// Reports from ripgit-ci
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PlannedJob {
    pub name: String,
    pub steps: Vec<String>,
}

/// A progress report from the runner, POSTed to `ci/report` as JSON.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Report {
    /// The pipeline was evaluated. `matched: false` means its triggers did not
    /// match this event, and the run is skipped.
    Plan {
        run: i64,
        name: String,
        matched: bool,
        #[serde(default)]
        jobs: Vec<PlannedJob>,
    },
    Job {
        run: i64,
        job: String,
        status: String,
    },
    Step {
        run: i64,
        job: String,
        index: i64,
        status: String,
        #[serde(default, rename = "exitCode")]
        exit_code: Option<i64>,
        #[serde(default, rename = "logKey")]
        log_key: Option<String>,
    },
    Run {
        run: i64,
        status: String,
        #[serde(default)]
        error: Option<String>,
    },
}

/// Apply a report. The outer error is storage; the inner one describes a
/// report that makes no sense (unknown run, bad status) for a 400.
pub fn apply_report(sql: &SqlStorage, report: Report) -> Result<std::result::Result<(), String>> {
    let status_ok = |s: &str| STATUSES.contains(&s);
    let now = now_secs() as f64;
    let run_number = match &report {
        Report::Plan { run, .. }
        | Report::Job { run, .. }
        | Report::Step { run, .. }
        | Report::Run { run, .. } => *run,
    };
    let Some(run) = get_run(sql, run_number)? else {
        return Ok(Err(format!("no run #{}", run_number)));
    };
    // A finished run is final; a late or retried report must not reopen it.
    if is_terminal(&run.status) {
        return Ok(Ok(()));
    }
    let n = v(run_number as f64);

    match report {
        Report::Plan {
            name, matched, jobs, ..
        } => {
            if !matched {
                sql.exec(
                    "UPDATE ci_runs SET name = ?, status = 'skipped', finished_at = ? WHERE number = ?",
                    vec![v(name), v(now), n],
                )?;
                return Ok(Ok(()));
            }
            sql.exec(
                "UPDATE ci_runs SET name = ?, status = 'queued' WHERE number = ?",
                vec![v(name), n.clone()],
            )?;
            for job in jobs {
                sql.exec(
                    "INSERT OR REPLACE INTO ci_jobs (run_id, name, status) VALUES (?, ?, 'queued')",
                    vec![n.clone(), v(job.name.as_str())],
                )?;
                for (idx, step) in job.steps.iter().enumerate() {
                    sql.exec(
                        "INSERT OR REPLACE INTO ci_steps (run_id, job, idx, name, status)
                         VALUES (?, ?, ?, ?, 'queued')",
                        vec![n.clone(), v(job.name.as_str()), v(idx as f64), v(step.as_str())],
                    )?;
                }
            }
        }
        Report::Job { job, status, .. } => {
            if !status_ok(&status) {
                return Ok(Err(format!("unknown status `{}`", status)));
            }
            let (started, finished) = stamps(&status, now);
            sql.exec(
                "UPDATE ci_jobs SET status = ?,
                    started_at = CASE WHEN ? > 0 AND started_at = 0 THEN ? ELSE started_at END,
                    finished_at = CASE WHEN ? > 0 THEN ? ELSE finished_at END
                 WHERE run_id = ? AND name = ?",
                vec![v(status.as_str()), v(started), v(started), v(finished), v(finished), n.clone(), v(job)],
            )?;
            if status == "running" {
                sql.exec(
                    "UPDATE ci_runs SET status = 'running' WHERE number = ? AND status IN ('planning', 'queued')",
                    vec![n],
                )?;
            }
        }
        Report::Step {
            job,
            index,
            status,
            exit_code,
            log_key,
            ..
        } => {
            if !status_ok(&status) {
                return Ok(Err(format!("unknown status `{}`", status)));
            }
            let (started, finished) = stamps(&status, now);
            sql.exec(
                "UPDATE ci_steps SET status = ?,
                    exit_code = CASE WHEN ? >= 0 THEN ? ELSE exit_code END,
                    log_key = CASE WHEN ? != '' THEN ? ELSE log_key END,
                    started_at = CASE WHEN ? > 0 AND started_at = 0 THEN ? ELSE started_at END,
                    finished_at = CASE WHEN ? > 0 THEN ? ELSE finished_at END
                 WHERE run_id = ? AND job = ? AND idx = ?",
                vec![
                    v(status.as_str()),
                    v(exit_code.unwrap_or(-1) as f64),
                    v(exit_code.unwrap_or(-1) as f64),
                    v(log_key.clone().unwrap_or_default()),
                    v(log_key.unwrap_or_default()),
                    v(started),
                    v(started),
                    v(finished),
                    v(finished),
                    n,
                    v(job),
                    v(index as f64),
                ],
            )?;
        }
        Report::Run { status, error, .. } => {
            if !status_ok(&status) {
                return Ok(Err(format!("unknown status `{}`", status)));
            }
            let finished = if is_terminal(&status) { now } else { 0.0 };
            sql.exec(
                "UPDATE ci_runs SET status = ?, error = ?,
                    finished_at = CASE WHEN ? > 0 THEN ? ELSE finished_at END
                 WHERE number = ?",
                vec![v(status.as_str()), v(error.unwrap_or_default()), v(finished), v(finished), n],
            )?;
        }
    }
    Ok(Ok(()))
}

/// (started_at, finished_at) stamps implied by moving to `status`; 0 = leave.
fn stamps(status: &str, now: f64) -> (f64, f64) {
    if status == "running" {
        (now, 0.0)
    } else if is_terminal(status) {
        (now, now)
    } else {
        (0.0, 0.0)
    }
}

// ---------------------------------------------------------------------------
// Triggering
// ---------------------------------------------------------------------------

/// Every ref and the commit it points at.
pub fn ref_snapshot(sql: &SqlStorage) -> Result<std::collections::HashMap<String, String>> {
    #[derive(Deserialize)]
    struct Row {
        name: String,
        commit_hash: String,
    }
    Ok(sql
        .exec("SELECT name, commit_hash FROM refs", None)?
        .to_array::<Row>()?
        .into_iter()
        .map(|r| (r.name, r.commit_hash))
        .collect())
}

/// Refs that point somewhere new after a push (created or moved, not deleted),
/// with their new commit. Comparing snapshots, rather than trusting the push
/// commands, leaves out updates git rejected.
pub fn moved_refs(
    before: &std::collections::HashMap<String, String>,
    after: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut moved: Vec<(String, String)> = after
        .iter()
        .filter(|(name, sha)| before.get(*name) != Some(*sha))
        .map(|(name, sha)| (name.clone(), sha.clone()))
        .collect();
    moved.sort();
    moved
}

/// What ripgit-ci needs to start a run.
#[derive(Serialize)]
struct StartRun<'a> {
    owner: &'a str,
    repo: &'a str,
    run: i64,
    sha: &'a str,
    #[serde(rename = "ref")]
    ref_name: &'a str,
    event: &'a str,
    pipeline: &'a str,
}

/// Start runs for the branches a push moved: one per pipeline file at each
/// branch's new commit. A no-op when the CI service is not bound, so ripgit
/// works without ripgit-ci deployed. Never fails the push: a run that cannot
/// be started is recorded as an error instead.
pub async fn trigger_push(
    env: &Env,
    sql: &SqlStorage,
    owner: &str,
    repo: &str,
    moved: &[(String, String)],
    actor: &str,
) -> Result<()> {
    let Ok(ci) = env.service("CI") else {
        return Ok(());
    };
    for (ref_name, sha) in moved {
        if !ref_name.starts_with("refs/heads/") {
            continue;
        }
        for pipeline in pipelines_at(sql, sha)? {
            let run = create_run(sql, &pipeline, "push", ref_name, sha, actor)?;
            let body = serde_json::to_string(&StartRun {
                owner,
                repo,
                run,
                sha,
                ref_name,
                event: "push",
                pipeline: &pipeline,
            })?;
            let headers = Headers::new();
            headers.set("Content-Type", "application/json")?;
            let mut init = RequestInit::new();
            init.with_method(Method::Post)
                .with_headers(headers)
                .with_body(Some(body.into()));
            let request = Request::new_with_init("https://ci.internal/runs", &init)?;
            match ci.fetch_request(request).await {
                Ok(resp) if resp.status_code() < 300 => {}
                Ok(mut resp) => {
                    let text = resp.text().await.unwrap_or_default();
                    fail_run(sql, run, &format!("CI refused the run: HTTP {} {}", resp.status_code(), text))?;
                }
                Err(e) => fail_run(sql, run, &format!("could not reach CI: {}", e))?,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_record_length_counts_itself() {
        let rec = TarWriter::pax_record("path", "a/b");
        assert_eq!(rec, "12 path=a/b\n");
        assert_eq!(rec.len(), 12);
        // Crossing a digit boundary: the length must still be exact.
        let long = "x".repeat(95);
        let rec = TarWriter::pax_record("path", &long);
        assert_eq!(rec.len().to_string(), rec.split(' ').next().unwrap());
    }

    #[test]
    fn header_checksum_and_fields_are_ustar() {
        let tar = TarWriter::new(0o1234);
        let h = tar.header("dir/file.txt", 0o644, 5, b'0', "");
        assert_eq!(&h[..12], b"dir/file.txt");
        assert_eq!(&h[100..108], b"0000644\0");
        assert_eq!(&h[124..136], b"00000000005\0");
        assert_eq!(&h[257..263], b"ustar\0");
        let mut check = h;
        check[148..156].copy_from_slice(b"        ");
        let sum: u32 = check.iter().map(|&b| b as u32).sum();
        let stored = std::str::from_utf8(&h[148..154]).unwrap();
        assert_eq!(u32::from_str_radix(stored, 8).unwrap(), sum);
    }

    #[test]
    fn long_paths_get_a_pax_header_and_data_is_block_padded() {
        let mut tar = TarWriter::new(0);
        let path = format!("{}/file", "d".repeat(120));
        tar.append(&path, 0o644, b'0', b"hello", "");
        let out = tar.finish();
        assert_eq!(out[156], b'x');
        assert_eq!(out.len() % 512, 0);
        let pax = String::from_utf8_lossy(&out[512..1024]);
        assert!(pax.contains(&format!("path={}\n", path)));
    }
}
