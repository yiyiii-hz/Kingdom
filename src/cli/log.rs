use crate::storage::Storage;
use crate::types::{ActionLogEntry, Job, JobStatus, Session};
use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;

pub fn run_log(
    workspace: PathBuf,
    detail: Option<String>,
    actions: bool,
    limit: Option<usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;
    let output = if let Some(job_id) = detail {
        let session = storage.load_session()?.ok_or_else(|| "no active session".to_string())?;
        render_job_detail(&storage, &session, &job_id)?
    } else if actions {
        render_actions_view(&storage.read_action_log(limit)?)
    } else {
        let session = storage.load_session()?.ok_or_else(|| "no active session".to_string())?;
        render_default_view(&session, &storage.read_action_log(None)?, Utc::now())
    };
    print!("{output}");
    Ok(())
}

fn render_default_view(
    session: &Session,
    entries: &[ActionLogEntry],
    now: DateTime<Utc>,
) -> String {
    let mut jobs = session.jobs.values().collect::<Vec<_>>();
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let failovers = failovers_by_job(entries);
    let mut output = String::new();
    for job in jobs {
        let completed_at = job
            .result
            .as_ref()
            .map(|result| format_time(result.completed_at))
            .unwrap_or_else(|| "--:--".to_string());
        let worker = job
            .worker_id
            .as_ref()
            .and_then(|worker_id| session.workers.get(worker_id))
            .map(|worker| format!("{}({})", capitalize(&worker.provider), worker.id))
            .or_else(|| {
                job.result.as_ref().and_then(|result| {
                    session.workers.get(&result.worker_id).map(|worker| {
                        format!("{}({})", capitalize(&worker.provider), worker.id)
                    })
                })
            })
            .unwrap_or_else(|| "-".to_string());
        let duration = format_job_duration(job, now);
        let _ = writeln!(
            output,
            "{:<8} {}  {:<24} {:<10} {:<5} {:<12} {}",
            job.id,
            status_icon(&job.status),
            truncate_text(&job.intent, 24),
            status_label(&job.status),
            completed_at,
            worker,
            duration
        );

        if let Some(job_failovers) = failovers.get(&job.id) {
            for failover in job_failovers {
                let _ = writeln!(
                    output,
                    "            ↳ failover: {}  {}  ({})",
                    failover.providers,
                    format_time(failover.timestamp),
                    failover.reason
                );
            }
        }
    }
    output
}

fn render_job_detail(storage: &Storage, session: &Session, job_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    let job = session
        .jobs
        .get(job_id)
        .ok_or_else(|| format!("job not found: {job_id}"))?;
    let entries = storage.read_action_log(None)?;
    let failovers = failovers_by_job(&entries);

    let mut output = String::new();
    let _ = writeln!(output, "{}  {}", job.id, job.intent);
    let _ = writeln!(output, "  created    {}  by manager", format_time(job.created_at));

    if let Some(worker_id) = &job.worker_id {
        if let Some(worker) = session.workers.get(worker_id) {
            let _ = writeln!(
                output,
                "  worker     {} {}",
                capitalize(&worker.provider),
                worker.id
            );
        }
    }

    if let Some(job_failovers) = failovers.get(job_id) {
        for failover in job_failovers {
            let _ = writeln!(
                output,
                "  failover   {}  {}",
                format_time(failover.timestamp),
                failover.providers
            );
        }
    }

    if let Some(result) = &job.result {
        let _ = writeln!(
            output,
            "  completed  {}  {} files changed",
            format_time(result.completed_at),
            result.changed_files.len()
        );
    }
    if let Some(branch) = &job.branch {
        let _ = writeln!(output, "  branch     {branch}");
    }

    output.push_str("\n  checkpoints:\n");
    for checkpoint in &job.checkpoints {
        let content = storage.load_checkpoint(&checkpoint.job_id, &checkpoint.id)?;
        let _ = writeln!(
            output,
            "    {}  [kingdom checkpoint] {}",
            format_time(checkpoint.created_at),
            truncate_text(&content.done, 60)
        );
    }

    Ok(output)
}

fn render_actions_view(entries: &[ActionLogEntry]) -> String {
    let mut output = String::new();
    for entry in entries {
        if entry.action == "compressed_summary" {
            let _ = writeln!(
                output,
                "{}  {:<12} {:<16} {}",
                format_time(entry.timestamp),
                entry.actor,
                entry.action,
                format_args!(
                    "{} ~ {}  [compressed: {} actions, {} tokens]",
                    entry.params["date_from"].as_str().unwrap_or("-"),
                    entry.params["date_to"].as_str().unwrap_or("-"),
                    entry.params["count"].as_u64().unwrap_or(0),
                    entry.params["tokens"].as_u64().unwrap_or(0)
                )
            );
            continue;
        }

        let _ = writeln!(
            output,
            "{}  {:<12} {:<16} {}",
            format_time(entry.timestamp),
            entry.actor,
            entry.action,
            summarize_params(entry)
        );
    }
    output
}

fn summarize_params(entry: &ActionLogEntry) -> String {
    let mut parts = Vec::new();
    if let Some(job_id) = entry.params.get("job_id").and_then(|value| value.as_str()) {
        parts.push(job_id.to_string());
    }
    if entry.action == "context.ping" {
        if let Some(tokens) = entry.params.get("token_count").and_then(|value| value.as_u64()) {
            parts.push(format!("token: {tokens}"));
        }
    }
    if let Some(reason) = entry.params.get("reason") {
        parts.push(summarize_json(reason));
    }
    if parts.is_empty() {
        parts.push(truncate_text(&summarize_json(&entry.params), 40));
    }
    truncate_text(&parts.join("  "), 80)
}

#[derive(Clone)]
struct FailoverLine {
    timestamp: DateTime<Utc>,
    providers: String,
    reason: String,
}

fn failovers_by_job(entries: &[ActionLogEntry]) -> HashMap<String, Vec<FailoverLine>> {
    let mut grouped: HashMap<String, Vec<FailoverLine>> = HashMap::new();
    for entry in entries {
        if !matches!(entry.action.as_str(), "failover.start" | "failover.triggered") {
            continue;
        }
        let Some(job_id) = entry.params.get("job_id").and_then(|value| value.as_str()) else {
            continue;
        };
        let from_provider = entry
            .params
            .get("from_provider")
            .and_then(|value| value.as_str())
            .map(capitalize)
            .unwrap_or_else(|| {
                entry.params
                    .get("provider")
                    .and_then(|value| value.as_str())
                    .map(capitalize)
                    .unwrap_or_else(|| "Unknown".to_string())
            });
        let to_provider = entry
            .params
            .get("to_provider")
            .or_else(|| entry.params.get("recommended_provider"))
            .and_then(|value| value.as_str())
            .map(capitalize)
            .unwrap_or_else(|| "Unknown".to_string());
        grouped.entry(job_id.to_string()).or_default().push(FailoverLine {
            timestamp: entry.timestamp,
            providers: format!("{from_provider}→{to_provider}"),
            reason: summarize_json(entry.params.get("reason").unwrap_or(&Value::Null)),
        });
    }
    grouped
}

fn status_icon(status: &JobStatus) -> &'static str {
    match status {
        JobStatus::Completed => "✓",
        JobStatus::Failed => "✗",
        JobStatus::Paused => "⚠",
        JobStatus::Running | JobStatus::Pending | JobStatus::Waiting | JobStatus::Cancelling => "⏳",
        JobStatus::Cancelled => "-",
    }
}

fn status_label(status: &JobStatus) -> &'static str {
    match status {
        JobStatus::Pending => "pending",
        JobStatus::Waiting => "waiting",
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
        JobStatus::Paused => "paused",
        JobStatus::Cancelling => "cancelling",
    }
}

fn format_job_duration(job: &Job, now: DateTime<Utc>) -> String {
    let end = job
        .result
        .as_ref()
        .map(|result| result.completed_at)
        .unwrap_or(now);
    let secs = end
        .signed_duration_since(job.created_at)
        .to_std()
        .unwrap_or_default()
        .as_secs();
    let prefix = if job.result.is_some() { "" } else { "~" };
    format!("{prefix}{}m{:02}s", secs / 60, secs % 60)
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text.chars().take(max_chars.saturating_sub(1)).collect::<String>();
    truncated.push('…');
    truncated
}

fn format_time(ts: DateTime<Utc>) -> String {
    ts.with_timezone(&Local).format("%H:%M").to_string()
}

fn summarize_json(value: &Value) -> String {
    match value {
        Value::Null => "-".to_string(),
        Value::String(s) => s.clone(),
        _ => truncate_text(&value.to_string(), 40),
    }
}

fn capitalize(provider: &str) -> String {
    let mut chars = provider.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => "Unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ActionLogEntry, CheckpointContent, CheckpointMeta, JobResult, Session, Worker, WorkerRole, WorkerStatus};
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;

    fn ts(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 6, hour, minute, 0).unwrap()
    }

    fn worker(id: &str, provider: &str) -> Worker {
        Worker {
            id: id.to_string(),
            provider: provider.to_string(),
            role: WorkerRole::Worker,
            status: WorkerStatus::Idle,
            job_id: None,
            pid: None,
            pane_id: String::new(),
            mcp_connected: true,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: None,
            last_progress: None,
            permissions: vec![],
            started_at: ts(10, 0),
        }
    }

    #[test]
    fn test_log_default_shows_jobs_sorted() {
        let mut session = Session {
            id: "sess".to_string(),
            workspace_path: ".".to_string(),
            workspace_hash: "hash".to_string(),
            manager_id: None,
            workers: [
                ("w1".to_string(), worker("w1", "codex")),
                ("w2".to_string(), worker("w2", "gemini")),
            ]
            .into_iter()
            .collect(),
            jobs: HashMap::new(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: crate::types::GitStrategy::None,
            available_providers: vec![],
            notification_mode: crate::types::NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: ts(9, 0),
        };
        session.jobs.insert(
            "job_001".to_string(),
            Job {
                id: "job_001".to_string(),
                intent: "First job".to_string(),
                status: JobStatus::Failed,
                worker_id: Some("w1".to_string()),
                depends_on: vec![],
                created_at: ts(10, 0),
                updated_at: ts(10, 0),
                branch: None,
                branch_start_commit: None,
                checkpoints: vec![],
                result: None,
                fail_count: 0,
                last_fail_at: None,
            },
        );
        session.jobs.insert(
            "job_002".to_string(),
            Job {
                id: "job_002".to_string(),
                intent: "Second job".to_string(),
                status: JobStatus::Completed,
                worker_id: Some("w2".to_string()),
                depends_on: vec![],
                created_at: ts(12, 0),
                updated_at: ts(12, 30),
                branch: None,
                branch_start_commit: None,
                checkpoints: vec![],
                result: Some(JobResult {
                    summary: "done".to_string(),
                    changed_files: vec![],
                    completed_at: ts(12, 30),
                    worker_id: "w2".to_string(),
                }),
                fail_count: 0,
                last_fail_at: None,
            },
        );

        let rendered = render_default_view(&session, &[], ts(13, 0));
        let first_line = rendered.lines().next().unwrap();
        assert!(first_line.contains("job_002"));
        assert!(rendered.contains("✓"));
        assert!(rendered.contains("✗"));
    }

    #[test]
    fn test_log_detail_missing_job() {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let session = Session {
            id: "sess".to_string(),
            workspace_path: ".".to_string(),
            workspace_hash: "hash".to_string(),
            manager_id: None,
            workers: HashMap::new(),
            jobs: HashMap::new(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: crate::types::GitStrategy::None,
            available_providers: vec![],
            notification_mode: crate::types::NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: ts(9, 0),
        };
        let err = render_job_detail(&storage, &session, "job_404").unwrap_err();
        assert_eq!(err.to_string(), "job not found: job_404");
    }

    #[test]
    fn test_log_actions_respects_limit() {
        let entries = [
            ActionLogEntry {
                timestamp: ts(10, 0),
                actor: "w1".to_string(),
                action: "job.progress".to_string(),
                params: serde_json::json!({"job_id":"job_001"}),
                result: None,
                error: None,
            },
            ActionLogEntry {
                timestamp: ts(10, 1),
                actor: "w1".to_string(),
                action: "job.complete".to_string(),
                params: serde_json::json!({"job_id":"job_001"}),
                result: None,
                error: None,
            },
        ];
        let rendered = render_actions_view(&entries[entries.len() - 2..]);
        assert_eq!(rendered.lines().count(), 2);
    }

    #[test]
    fn detail_renders_checkpoints_from_files() {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let checkpoint = CheckpointContent {
            id: "ckpt_1".to_string(),
            job_id: "job_001".to_string(),
            created_at: ts(11, 0),
            done: "Checkpoint summary".to_string(),
            abandoned: String::new(),
            in_progress: String::new(),
            remaining: String::new(),
            pitfalls: String::new(),
            git_commit: None,
        };
        storage.save_checkpoint(&checkpoint).unwrap();
        let mut session = Session {
            id: "sess".to_string(),
            workspace_path: ".".to_string(),
            workspace_hash: "hash".to_string(),
            manager_id: None,
            workers: HashMap::new(),
            jobs: HashMap::new(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: crate::types::GitStrategy::None,
            available_providers: vec![],
            notification_mode: crate::types::NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: ts(9, 0),
        };
        session.jobs.insert(
            "job_001".to_string(),
            Job {
                id: "job_001".to_string(),
                intent: "Intent".to_string(),
                status: JobStatus::Running,
                worker_id: None,
                depends_on: vec![],
                created_at: ts(10, 0),
                updated_at: ts(10, 0),
                branch: Some("kingdom/job_001".to_string()),
                branch_start_commit: None,
                checkpoints: vec![CheckpointMeta {
                    id: "ckpt_1".to_string(),
                    job_id: "job_001".to_string(),
                    created_at: ts(11, 0),
                    git_commit: None,
                }],
                result: None,
                fail_count: 0,
                last_fail_at: None,
            },
        );

        let rendered = render_job_detail(&storage, &session, "job_001").unwrap();
        assert!(rendered.contains("Checkpoint summary"));
    }
}
