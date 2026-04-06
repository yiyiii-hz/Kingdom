use crate::storage::Storage;
use crate::types::{CheckpointContent, JobStatus, Session};
use chrono::{DateTime, Utc};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub fn run_clean(
    workspace: PathBuf,
    dry_run: bool,
    all: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;
    let plan = build_clean_plan(&storage, Utc::now(), all)?;
    if plan.is_empty() {
        println!("没有需要清理的内容。");
        return Ok(());
    }

    print!("{}", render_clean_plan(&plan));
    if dry_run {
        return Ok(());
    }

    print!("\n继续清理？[y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if !matches!(line.trim(), "y" | "Y") {
        return Ok(());
    }

    execute_clean_plan(&storage, &plan)?;
    Ok(())
}

#[derive(Default)]
struct CleanPlan {
    checkpoint_jobs: Vec<CheckpointCleanup>,
    archive_jobs: Vec<ArchiveCleanup>,
    action_log: Option<ActionLogCleanup>,
    total_bytes: u64,
}

impl CleanPlan {
    fn is_empty(&self) -> bool {
        self.checkpoint_jobs.is_empty() && self.archive_jobs.is_empty() && self.action_log.is_none()
    }
}

struct CheckpointCleanup {
    job_id: String,
    count: usize,
    bytes: u64,
    oldest: DateTime<Utc>,
    cutoff: DateTime<Utc>,
}

struct ArchiveCleanup {
    job_id: String,
    bytes: u64,
    completed_at: DateTime<Utc>,
}

struct ActionLogCleanup {
    cutoff: DateTime<Utc>,
    date_from: DateTime<Utc>,
    date_to: DateTime<Utc>,
    bytes: u64,
}

fn build_clean_plan(
    storage: &Storage,
    now: DateTime<Utc>,
    all: bool,
) -> Result<CleanPlan, Box<dyn std::error::Error>> {
    let mut plan = CleanPlan::default();
    let session = storage.load_session()?.unwrap_or_else(empty_session);
    let checkpoint_cutoff = now - chrono::Duration::days(7);
    let archive_cutoff = now - chrono::Duration::days(90);
    let action_cutoff = now - chrono::Duration::days(30);

    for job in session
        .jobs
        .values()
        .filter(|job| job.status == JobStatus::Completed)
    {
        let mut deletable = Vec::new();
        let files = storage.list_checkpoint_files(&job.id)?;
        let last = files.last().cloned();
        for path in files {
            if last.as_ref() == Some(&path) {
                continue;
            }
            let checkpoint: CheckpointContent = serde_json::from_slice(&std::fs::read(&path)?)?;
            if all || checkpoint.created_at < checkpoint_cutoff {
                deletable.push((path, checkpoint.created_at));
            }
        }
        if !deletable.is_empty() {
            let bytes = deletable
                .iter()
                .map(|(path, _)| file_size(path))
                .sum::<u64>();
            plan.total_bytes += bytes;
            plan.checkpoint_jobs.push(CheckpointCleanup {
                job_id: job.id.clone(),
                count: deletable.len(),
                bytes,
                oldest: deletable
                    .iter()
                    .map(|(_, created_at)| *created_at)
                    .min()
                    .unwrap_or(now),
                cutoff: if all {
                    now + chrono::Duration::days(3650)
                } else {
                    checkpoint_cutoff
                },
            });
        }

        if let Some(result) = &job.result {
            let result_path = storage.root.join("jobs").join(&job.id).join("result.json");
            if result_path.exists() && (all || result.completed_at < archive_cutoff) {
                let bytes = file_size(&result_path);
                plan.total_bytes += bytes;
                plan.archive_jobs.push(ArchiveCleanup {
                    job_id: job.id.clone(),
                    bytes,
                    completed_at: result.completed_at,
                });
            }
        }
    }

    let entries = storage.read_action_log(None)?;
    let old_entries = entries
        .iter()
        .filter(|entry| {
            if all {
                true
            } else {
                entry.timestamp < action_cutoff
            }
        })
        .collect::<Vec<_>>();
    if let (Some(first), Some(last)) = (old_entries.first(), old_entries.last()) {
        let bytes = file_size(&storage.root.join("logs").join("action.jsonl"));
        plan.total_bytes += bytes;
        plan.action_log = Some(ActionLogCleanup {
            cutoff: if all { now } else { action_cutoff },
            date_from: first.timestamp,
            date_to: last.timestamp,
            bytes,
        });
    }

    Ok(plan)
}

fn execute_clean_plan(
    storage: &Storage,
    plan: &CleanPlan,
) -> Result<(), Box<dyn std::error::Error>> {
    for item in &plan.checkpoint_jobs {
        storage.delete_old_checkpoints(&item.job_id, item.cutoff)?;
    }
    for item in &plan.archive_jobs {
        storage.archive_job(&item.job_id)?;
    }
    if let Some(action_log) = &plan.action_log {
        storage.compress_action_log(action_log.cutoff)?;
    }
    Ok(())
}

fn render_clean_plan(plan: &CleanPlan) -> String {
    let mut output = String::from("将清理以下内容：\n\n");

    if !plan.checkpoint_jobs.is_empty() {
        output.push_str("  已完成 job 中间 checkpoint（>7天）\n");
        for item in &plan.checkpoint_jobs {
            let _ = writeln!(
                output,
                "    {}  {} 个 checkpoint  · {}  {}",
                item.job_id,
                item.count,
                format_bytes(item.bytes),
                item.oldest.format("%Y-%m-%d")
            );
        }
        output.push('\n');
    }

    if !plan.archive_jobs.is_empty() {
        output.push_str("  归档已完成 job 结果（>90天）\n");
        for item in &plan.archive_jobs {
            let _ = writeln!(
                output,
                "    {}  · {}  {}",
                item.job_id,
                format_bytes(item.bytes),
                item.completed_at.format("%Y-%m-%d")
            );
        }
        output.push('\n');
    }

    if let Some(item) = &plan.action_log {
        output.push_str("  压缩旧 action log（>30天）\n");
        let _ = writeln!(
            output,
            "    {} ~ {}  · {} → 约 0.5 MB",
            item.date_from.format("%Y-%m-%d"),
            item.date_to.format("%Y-%m-%d"),
            format_bytes(item.bytes)
        );
        output.push('\n');
    }

    let _ = writeln!(output, "合计释放：约 {}", format_bytes(plan.total_bytes));
    output
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn empty_session() -> Session {
    Session {
        id: "empty".to_string(),
        workspace_path: ".".to_string(),
        workspace_hash: "hash".to_string(),
        manager_id: None,
        workers: Default::default(),
        jobs: Default::default(),
        notes: vec![],
        worker_seq: 0,
        job_seq: 0,
        request_seq: 0,
        git_strategy: crate::types::GitStrategy::None,
        available_providers: vec![],
        notification_mode: crate::types::NotificationMode::Poll,
        pending_requests: Default::default(),
        pending_failovers: Default::default(),
        provider_stability: Default::default(),
        created_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ActionLogEntry, Job, JobResult};
    use chrono::TimeZone;

    fn ts(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    fn save_completed_job(storage: &Storage, now: DateTime<Utc>) {
        let mut session = empty_session();
        session.jobs.insert(
            "job_001".to_string(),
            Job {
                id: "job_001".to_string(),
                intent: "done".to_string(),
                status: JobStatus::Completed,
                worker_id: None,
                depends_on: vec![],
                created_at: now - chrono::Duration::days(100),
                updated_at: now - chrono::Duration::days(100),
                branch: None,
                branch_start_commit: None,
                checkpoints: vec![],
                result: Some(JobResult {
                    summary: "done".to_string(),
                    changed_files: vec![],
                    completed_at: now - chrono::Duration::days(100),
                    worker_id: "w1".to_string(),
                }),
                fail_count: 0,
                last_fail_at: None,
            },
        );
        storage.save_session(&session).unwrap();
        storage
            .save_result(
                "job_001",
                &JobResult {
                    summary: "done".to_string(),
                    changed_files: vec![],
                    completed_at: now - chrono::Duration::days(100),
                    worker_id: "w1".to_string(),
                },
            )
            .unwrap();
    }

    #[test]
    fn test_clean_dry_run_does_not_modify() {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let now = ts(2026, 4, 6);
        save_completed_job(&storage, now);

        run_clean(temp.path().to_path_buf(), true, false).unwrap();
        assert!(storage.root.join("jobs/job_001/result.json").exists());
        assert!(!storage.root.join("archive/job_001/result.json").exists());
    }

    #[test]
    fn test_clean_archives_old_jobs() {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let now = ts(2026, 4, 6);
        save_completed_job(&storage, now);

        let plan = build_clean_plan(&storage, now, false).unwrap();
        execute_clean_plan(&storage, &plan).unwrap();
        assert!(storage.root.join("archive/job_001/result.json").exists());
    }

    #[test]
    fn test_clean_compresses_old_action_log() {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        storage
            .append_action_log(&ActionLogEntry {
                timestamp: ts(2026, 3, 1),
                actor: "w1".to_string(),
                action: "context.ping".to_string(),
                params: serde_json::json!({"token_count": 100}),
                result: None,
                error: None,
            })
            .unwrap();

        let plan = build_clean_plan(&storage, ts(2026, 4, 6), false).unwrap();
        execute_clean_plan(&storage, &plan).unwrap();
        let entries = storage.read_action_log(None).unwrap();
        assert_eq!(entries[0].action, "compressed_summary");
    }
}
