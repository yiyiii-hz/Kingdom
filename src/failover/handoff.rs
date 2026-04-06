use crate::storage::Storage;
use crate::types::{ActionLogEntry, CheckpointContent, HandoffBrief, Session, Worker};
use std::path::Path;

pub async fn build_handoff_brief(
    session: &Session,
    worker: &Worker,
    storage: &Storage,
    workspace_path: &Path,
) -> Option<HandoffBrief> {
    let job_id = worker.job_id.as_ref()?;
    let job = session.jobs.get(job_id)?;
    let checkpoint = load_latest_checkpoint(storage, job_id);
    let changed_files = git_changed_files(workspace_path).await;

    Some(HandoffBrief {
        job_id: job.id.clone(),
        original_intent: job.intent.clone(),
        done: checkpoint
            .as_ref()
            .map(|c| c.done.clone())
            .unwrap_or_else(|| "No checkpoint available".to_string()),
        in_progress: checkpoint
            .as_ref()
            .map(|c| c.in_progress.clone())
            .unwrap_or_else(|| "Unknown".to_string()),
        remaining: checkpoint
            .as_ref()
            .map(|c| c.remaining.clone())
            .unwrap_or_else(|| "Unknown".to_string()),
        pitfalls: checkpoint
            .as_ref()
            .map(|c| c.pitfalls.clone())
            .unwrap_or_else(|| "None recorded".to_string()),
        possibly_incomplete_files: changed_files.clone(),
        changed_files,
    })
}

pub fn build_manager_recovery_context(
    session: &Session,
    storage: &Storage,
    workspace_path: &Path,
) -> String {
    let kingdom_md = std::fs::read_to_string(workspace_path.join("KINGDOM.md")).unwrap_or_default();
    let logs = storage.read_action_log(Some(20)).unwrap_or_default();

    let mut output = String::new();
    output.push_str("# Manager Recovery Context\n\n");
    if !kingdom_md.trim().is_empty() {
        output.push_str("## KINGDOM.md\n");
        output.push_str(&kingdom_md);
        output.push_str("\n\n");
    }

    output.push_str("## Workspace Notes\n");
    if session.notes.is_empty() {
        output.push_str("- None\n\n");
    } else {
        for note in &session.notes {
            output.push_str(&format!("- {}\n", note.content));
        }
        output.push('\n');
    }

    output.push_str("## Jobs\n");
    if session.jobs.is_empty() {
        output.push_str("- None\n\n");
    } else {
        let mut jobs: Vec<_> = session.jobs.values().collect();
        jobs.sort_by_key(|job| job.id.clone());
        for job in jobs {
            output.push_str(&format!("- {}: {:?}\n", job.id, job.status));
        }
        output.push('\n');
    }

    output.push_str("## Workers\n");
    if session.workers.is_empty() {
        output.push_str("- None\n\n");
    } else {
        let mut workers: Vec<_> = session.workers.values().collect();
        workers.sort_by_key(|worker| worker.id.clone());
        for worker in workers {
            output.push_str(&format!(
                "- {}: role={:?} provider={} status={:?} job={}\n",
                worker.id,
                worker.role,
                worker.provider,
                worker.status,
                worker.job_id.as_deref().unwrap_or("-")
            ));
        }
        output.push('\n');
    }

    output.push_str("## Pending Queues\n");
    if session.pending_requests.is_empty() && session.pending_failovers.is_empty() {
        output.push_str("- None\n\n");
    } else {
        for request in session.pending_requests.values() {
            output.push_str(&format!(
                "- request {}: job={} blocking={} answered={}\n",
                request.id, request.job_id, request.blocking, request.answered
            ));
        }
        for failover in session.pending_failovers.values() {
            output.push_str(&format!(
                "- failover {}: job={} reason={:?} status={:?}\n",
                failover.worker_id, failover.job_id, failover.reason, failover.status
            ));
        }
        output.push('\n');
    }

    output.push_str("## Recent Action Log\n");
    append_logs(&mut output, &logs);
    output
}

fn append_logs(output: &mut String, logs: &[ActionLogEntry]) {
    if logs.is_empty() {
        output.push_str("- None\n");
        return;
    }
    for entry in logs {
        output.push_str(&format!(
            "- {} {} {}\n",
            entry.timestamp.to_rfc3339(),
            entry.actor,
            entry.action
        ));
    }
}

fn load_latest_checkpoint(storage: &Storage, job_id: &str) -> Option<CheckpointContent> {
    let session = storage.load_session().ok().flatten()?;
    let job = session.jobs.get(job_id)?;
    let checkpoint = job.checkpoints.last()?;
    storage.load_checkpoint(job_id, &checkpoint.id).ok()
}

async fn git_changed_files(workspace_path: &Path) -> Vec<String> {
    let output = tokio::process::Command::new("git")
        .args(["diff", "--name-only"])
        .current_dir(workspace_path)
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use crate::types::{
        CheckpointMeta, GitStrategy, Job, JobStatus, NoteScope, NotificationMode, PendingFailover,
        PendingFailoverStatus, PendingRequest, Session, Worker, WorkerRole, WorkerStatus,
        WorkspaceNote,
    };
    use chrono::Utc;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn ts() -> chrono::DateTime<Utc> {
        Utc::now()
    }

    fn session(root: &Path) -> Session {
        let worker = Worker {
            id: "w1".to_string(),
            index: 1,
            provider: "codex".to_string(),
            role: WorkerRole::Worker,
            status: WorkerStatus::Running,
            job_id: Some("job_001".to_string()),
            pid: None,
            pane_id: "%1".to_string(),
            mcp_connected: true,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: None,
            last_progress: None,
            permissions: vec![],
            started_at: ts(),
        };
        let job = Job {
            id: "job_001".to_string(),
            intent: "Implement handoff".to_string(),
            status: JobStatus::Running,
            worker_id: Some("w1".to_string()),
            depends_on: vec![],
            created_at: ts(),
            updated_at: ts(),
            branch: None,
            branch_start_commit: None,
            checkpoints: vec![CheckpointMeta {
                id: "ckpt_1".to_string(),
                job_id: "job_001".to_string(),
                created_at: ts(),
                git_commit: None,
            }],
            result: None,
            fail_count: 0,
            last_fail_at: None,
        };
        Session {
            id: "sess".to_string(),
            workspace_path: root.display().to_string(),
            workspace_hash: "abc".to_string(),
            manager_id: None,
            workers: [("w1".to_string(), worker)].into_iter().collect(),
            jobs: [("job_001".to_string(), job)].into_iter().collect(),
            notes: vec![WorkspaceNote {
                id: "note_1".to_string(),
                content: "Keep tests deterministic".to_string(),
                scope: NoteScope::Global,
                created_at: ts(),
            }],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: GitStrategy::None,
            available_providers: vec![],
            notification_mode: NotificationMode::Poll,
            pending_requests: [(
                "req_1".to_string(),
                PendingRequest {
                    id: "req_1".to_string(),
                    job_id: "job_001".to_string(),
                    worker_id: "w1".to_string(),
                    question: "Proceed?".to_string(),
                    blocking: true,
                    answer: None,
                    answered: false,
                    created_at: ts(),
                    answered_at: None,
                },
            )]
            .into_iter()
            .collect(),
            pending_failovers: [(
                "w1".to_string(),
                PendingFailover {
                    worker_id: "w1".to_string(),
                    job_id: "job_001".to_string(),
                    reason: crate::types::FailoverReason::HeartbeatTimeout,
                    handoff_brief: crate::types::HandoffBrief {
                        job_id: "job_001".to_string(),
                        original_intent: "Implement handoff".to_string(),
                        done: "done".to_string(),
                        in_progress: "progress".to_string(),
                        remaining: "remaining".to_string(),
                        pitfalls: "pitfalls".to_string(),
                        possibly_incomplete_files: vec![],
                        changed_files: vec![],
                    },
                    recommended_provider: Some("claude".to_string()),
                    created_at: ts(),
                    status: PendingFailoverStatus::WaitingConfirmation,
                },
            )]
            .into_iter()
            .collect(),
            provider_stability: HashMap::new(),
            created_at: ts(),
        }
    }

    #[test]
    fn manager_recovery_context_contains_notes_jobs_and_logs() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let session = session(temp.path());
        storage.save_session(&session).unwrap();
        storage
            .append_action_log(&ActionLogEntry {
                timestamp: ts(),
                actor: "kingdom".to_string(),
                action: "failover".to_string(),
                params: serde_json::json!({}),
                result: None,
                error: None,
            })
            .unwrap();

        let context = build_manager_recovery_context(&session, &storage, temp.path());
        assert!(context.contains("Workspace Notes"));
        assert!(context.contains("Keep tests deterministic"));
        assert!(context.contains("job_001: Running"));
        assert!(context.contains("Workers"));
        assert!(context.contains("Pending Queues"));
        assert!(context.contains("request req_1"));
        assert!(context.contains("failover w1"));
        assert!(context.contains("failover"));
    }

    #[tokio::test]
    async fn build_handoff_brief_uses_async_git_diff() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let session = session(temp.path());
        storage.save_session(&session).unwrap();
        std::fs::write(temp.path().join("file.txt"), "hello").unwrap();

        let brief = build_handoff_brief(
            &session,
            session.workers.get("w1").unwrap(),
            &storage,
            temp.path(),
        )
        .await
        .unwrap();
        assert_eq!(brief.job_id, "job_001");
    }
}
