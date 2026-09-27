use super::*;
use crate::ci::{JobRow, RunRow, StepRow};

/// A failed step's log tail, fetched by the caller so the page can show why.
pub(crate) struct LogTail {
    pub job: String,
    pub idx: i64,
    pub text: String,
}

fn status_badge(status: &str) -> String {
    let color = match status {
        "success" => "#1a7f37",
        "failure" | "error" => "#cf222e",
        "running" => "#9a6700",
        "skipped" | "cancelled" => "#656d76",
        _ => "#0969da",
    };
    format!(
        r#"<span style="display:inline-block;padding:1px 8px;border-radius:10px;font-size:12px;font-weight:600;color:#fff;background:{color}">{}</span>"#,
        html_escape(status)
    )
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn branch(ref_name: &str) -> &str {
    ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name)
}

fn run_title(run: &RunRow) -> String {
    if run.name.is_empty() {
        run.pipeline.clone()
    } else {
        run.name.clone()
    }
}

fn duration(start: i64, end: i64) -> String {
    if start == 0 || end == 0 || end < start {
        return String::new();
    }
    let s = end - start;
    if s < 60 {
        format!("{}s", s)
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

fn log_path(owner: &str, repo: &str, run: i64, job: &str, idx: i64) -> String {
    format!(
        "/{}/{}/actions/{}/logs/{}/{}",
        owner,
        repo,
        run,
        url_path_segment(job),
        idx
    )
}

/// Percent-encode a path segment (job names are user-chosen).
fn url_path_segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Run list
// ---------------------------------------------------------------------------

pub fn page_actions(
    owner: &str,
    repo_name: &str,
    default_branch: &str,
    runs: &[RunRow],
    viewer: Viewer<'_>,
) -> Result<Response> {
    let body = if runs.is_empty() {
        format!(
            r#"<div class="empty-repo"><h2>No runs yet</h2>
<p>Add a pipeline in <code>{dir}/</code> and push. Each <code>.ts</code> file there runs on every push its triggers match.</p>
<pre class="push-cmd">// {dir}/test.ts
import {{ pipeline }} from "@ripgit/ci";

export default pipeline({{
  name: "test",
  on: {{ push: {{ branches: ["main"] }} }},
  jobs: {{
    test: {{ steps: [{{ name: "test", run: "cargo test" }}] }},
  }},
}});</pre></div>"#,
            dir = crate::ci::PIPELINES_DIR
        )
    } else {
        let rows: String = runs
            .iter()
            .map(|r| {
                format!(
                    r#"<tr><td>{badge}</td><td><a href="/{o}/{rn}/actions/{n}">{title}</a> <span class="muted">#{n}</span></td><td>{branch}</td><td><a href="/{o}/{rn}/commit/{sha}"><code>{short}</code></a></td><td>{actor}</td><td class="muted">{when}</td></tr>"#,
                    badge = status_badge(&r.status),
                    o = html_escape(owner),
                    rn = html_escape(repo_name),
                    n = r.number,
                    title = html_escape(&run_title(r)),
                    branch = html_escape(branch(&r.ref_name)),
                    sha = html_escape(&r.sha),
                    short = html_escape(short(&r.sha)),
                    actor = html_escape(&r.actor),
                    when = format_time(r.created_at),
                )
            })
            .collect();
        format!(
            r#"<table class="runs" style="width:100%;border-collapse:collapse"><thead><tr><th></th><th>Run</th><th>Branch</th><th>Commit</th><th>By</th><th>When</th></tr></thead><tbody>{rows}</tbody></table>"#
        )
    };
    html_response(&layout(
        "Actions",
        owner,
        repo_name,
        default_branch,
        viewer,
        &format!("<h2>Actions</h2>{}", body),
    ))
}

pub fn page_actions_markdown(
    owner: &str,
    repo_name: &str,
    runs: &[RunRow],
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let mut md = format!("# {}/{} actions\n\n", owner, repo_name);
    if runs.is_empty() {
        md.push_str(&format!(
            "No runs yet. Pipelines are `.ts` files in `{}/`; each runs on pushes its triggers match.\n",
            crate::ci::PIPELINES_DIR
        ));
    }
    for r in runs {
        md.push_str(&format!(
            "- #{} `{}` - **{}** on `{}` at `{}` by `{}` - `/{}/{}/actions/{}`\n",
            r.number,
            run_title(r),
            r.status,
            branch(&r.ref_name),
            short(&r.sha),
            r.actor,
            owner,
            repo_name,
            r.number
        ));
    }
    md.push_str(&presentation::render_hints_section(&[
        presentation::text_navigation_hint(*selection),
    ]));
    presentation::markdown_response(&md, selection)
}

// ---------------------------------------------------------------------------
// One run
// ---------------------------------------------------------------------------

pub fn page_run(
    owner: &str,
    repo_name: &str,
    default_branch: &str,
    run: &RunRow,
    jobs: &[JobRow],
    steps: &[StepRow],
    tails: &[LogTail],
    viewer: Viewer<'_>,
) -> Result<Response> {
    let mut html = format!(
        r#"<h2>{title} <span class="muted">#{n}</span> {badge}</h2>
<p class="muted"><code>{pipeline}</code> · {event} to <strong>{branch}</strong> at <a href="/{o}/{rn}/commit/{sha}"><code>{short}</code></a> by {actor} · {when}</p>"#,
        title = html_escape(&run_title(run)),
        n = run.number,
        badge = status_badge(&run.status),
        pipeline = html_escape(&run.pipeline),
        event = html_escape(&run.event),
        branch = html_escape(branch(&run.ref_name)),
        o = html_escape(owner),
        rn = html_escape(repo_name),
        sha = html_escape(&run.sha),
        short = html_escape(short(&run.sha)),
        actor = html_escape(&run.actor),
        when = format_time(run.created_at),
    );
    if !run.error.is_empty() {
        html.push_str(&format!(
            r#"<pre class="push-cmd" style="border-color:#cf222e">{}</pre>"#,
            html_escape(&run.error)
        ));
    }
    if run.status == "skipped" {
        html.push_str(r#"<p class="muted">This pipeline's triggers did not match this push.</p>"#);
    }
    for job in jobs {
        html.push_str(&format!(
            r#"<section class="settings-section"><h3>{badge} {name} <span class="muted">{dur}</span></h3><ol style="margin-left:20px">"#,
            badge = status_badge(&job.status),
            name = html_escape(&job.name),
            dur = duration(job.started_at, job.finished_at),
        ));
        for step in steps.iter().filter(|s| s.job == job.name) {
            let log = if step.log_key.is_empty() {
                String::new()
            } else {
                format!(
                    r#" · <a href="{}">log</a>"#,
                    html_escape(&log_path(owner, repo_name, run.number, &job.name, step.idx))
                )
            };
            let exit = if step.exit_code >= 0 && step.status != "success" {
                format!(" · exit {}", step.exit_code)
            } else {
                String::new()
            };
            html.push_str(&format!(
                r#"<li>{badge} {name} <span class="muted">{dur}{exit}</span>{log}"#,
                badge = status_badge(&step.status),
                name = html_escape(&step.name),
                dur = duration(step.started_at, step.finished_at),
            ));
            if let Some(tail) = tails.iter().find(|t| t.job == step.job && t.idx == step.idx) {
                html.push_str(&format!(
                    r#"<pre class="push-cmd" style="max-height:420px;overflow:auto">{}</pre>"#,
                    html_escape(&tail.text)
                ));
            }
            html.push_str("</li>");
        }
        html.push_str("</ol></section>");
    }
    html_response(&layout(
        &format!("{} #{}", run_title(run), run.number),
        owner,
        repo_name,
        default_branch,
        viewer,
        &html,
    ))
}

pub fn page_run_markdown(
    owner: &str,
    repo_name: &str,
    run: &RunRow,
    jobs: &[JobRow],
    steps: &[StepRow],
    tails: &[LogTail],
    selection: &NegotiatedRepresentation,
) -> Result<Response> {
    let mut md = format!(
        "# {} #{}\n\n- Status: **{}**\n- Pipeline: `{}`\n- Event: `{}` to `{}` at `{}` by `{}`\n",
        run_title(run),
        run.number,
        run.status,
        run.pipeline,
        run.event,
        branch(&run.ref_name),
        run.sha,
        run.actor
    );
    if !run.error.is_empty() {
        md.push_str(&format!("- Error: {}\n", run.error));
    }
    for job in jobs {
        md.push_str(&format!("\n## Job `{}`: {}\n", job.name, job.status));
        for step in steps.iter().filter(|s| s.job == job.name) {
            md.push_str(&format!("- {}. `{}`: {}", step.idx + 1, step.name, step.status));
            if step.exit_code >= 0 {
                md.push_str(&format!(" (exit {})", step.exit_code));
            }
            if !step.log_key.is_empty() {
                md.push_str(&format!(
                    " - log: `{}`",
                    log_path(owner, repo_name, run.number, &job.name, step.idx)
                ));
            }
            md.push('\n');
            if let Some(tail) = tails.iter().find(|t| t.job == step.job && t.idx == step.idx) {
                md.push_str("\n```\n");
                md.push_str(&tail.text);
                md.push_str("\n```\n");
            }
        }
    }
    md.push_str(&presentation::render_hints_section(&[
        presentation::text_navigation_hint(*selection),
        Hint::new("Log paths return the full step output as text/plain."),
    ]));
    presentation::markdown_response(&md, selection)
}
