use crate::storage::Storage;
use crate::types::Session;
use std::path::PathBuf;

pub fn run_open(workspace: PathBuf, target: String) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;
    let session = storage.load_session()?.ok_or("no active session")?;
    let pane_id = resolve_pane_id(&session, &target)
        .ok_or_else(|| format!("找不到 worker 或 job：{target}"))?;

    if pane_id.is_empty() {
        println!("pane 已关闭（job 已结束）。");
        return Ok(());
    }

    let status = std::process::Command::new("tmux")
        .args(["select-pane", "-t", &pane_id])
        .status()?;
    if !status.success() {
        println!("pane 已关闭（job 已结束）。");
    }
    Ok(())
}

fn resolve_pane_id(session: &Session, target: &str) -> Option<String> {
    if let Some(worker) = session.workers.get(target) {
        return Some(worker.pane_id.clone());
    }
    if let Some(job) = session.jobs.get(target) {
        if let Some(worker_id) = &job.worker_id {
            return session
                .workers
                .get(worker_id)
                .map(|worker| worker.pane_id.clone());
        }
        return Some(String::new());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        GitStrategy, Job, JobStatus, NotificationMode, Session, Worker, WorkerRole, WorkerStatus,
    };
    use chrono::Utc;
    use std::collections::HashMap;

    fn ts() -> chrono::DateTime<Utc> {
        Utc::now()
    }

    fn session() -> Session {
        Session {
            id: "sess_1".to_string(),
            workspace_path: ".".to_string(),
            workspace_hash: "abc".to_string(),
            manager_id: None,
            workers: [(
                "w1".to_string(),
                Worker {
                    id: "w1".to_string(),
                    index: 1,
                    provider: "codex".to_string(),
                    role: WorkerRole::Worker,
                    status: WorkerStatus::Idle,
                    job_id: None,
                    pid: None,
                    pane_id: "%1".to_string(),
                    mcp_connected: true,
                    context_usage_pct: None,
                    token_count: None,
                    last_heartbeat: None,
                    last_progress: None,
                    permissions: vec![],
                    started_at: ts(),
                },
            )]
            .into_iter()
            .collect(),
            jobs: [
                (
                    "job_001".to_string(),
                    Job {
                        id: "job_001".to_string(),
                        intent: "demo".to_string(),
                        status: JobStatus::Running,
                        worker_id: Some("w1".to_string()),
                        depends_on: vec![],
                        created_at: ts(),
                        updated_at: ts(),
                        branch: None,
                        branch_start_commit: None,
                        checkpoints: vec![],
                        result: None,
                        fail_count: 0,
                        last_fail_at: None,
                    },
                ),
                (
                    "job_002".to_string(),
                    Job {
                        id: "job_002".to_string(),
                        intent: "done".to_string(),
                        status: JobStatus::Completed,
                        worker_id: None,
                        depends_on: vec![],
                        created_at: ts(),
                        updated_at: ts(),
                        branch: None,
                        branch_start_commit: None,
                        checkpoints: vec![],
                        result: None,
                        fail_count: 0,
                        last_fail_at: None,
                    },
                ),
            ]
            .into_iter()
            .collect(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 2,
            request_seq: 0,
            git_strategy: GitStrategy::None,
            available_providers: vec!["codex".to_string()],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: ts(),
        }
    }

    #[test]
    fn test_open_resolve_by_worker_id() {
        assert_eq!(resolve_pane_id(&session(), "w1"), Some("%1".to_string()));
    }

    #[test]
    fn test_open_resolve_by_job_id() {
        assert_eq!(
            resolve_pane_id(&session(), "job_001"),
            Some("%1".to_string())
        );
    }

    #[test]
    fn test_open_resolve_job_no_worker() {
        assert_eq!(resolve_pane_id(&session(), "job_002"), Some(String::new()));
    }

    #[test]
    fn test_open_resolve_unknown() {
        assert_eq!(resolve_pane_id(&session(), "missing"), None);
    }
}
