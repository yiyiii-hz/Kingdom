use crate::storage::Storage;
use crate::types::{GitStrategy, Job};
use std::path::PathBuf;

pub fn run_job_diff(
    workspace: PathBuf,
    job_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;
    let session = storage.load_session()?.ok_or("no active session")?;
    let job = session
        .jobs
        .get(&job_id)
        .ok_or_else(|| format!("job not found: {job_id}"))?;

    if matches!(session.git_strategy, GitStrategy::None) {
        println!("该 job 在非 git 模式下运行，无 diff 记录。");
        return Ok(());
    }

    let start_commit = match &job.branch_start_commit {
        Some(commit) => commit.clone(),
        None => {
            println!("job {job_id} 尚未产生 git commit（可能未完成）。");
            return Ok(());
        }
    };

    let mut command = std::process::Command::new("git");
    command.args(["-C", workspace.to_str().unwrap_or("."), "diff", &start_commit, "HEAD"]);
    if let Some(files) = changed_files(job) {
        command.arg("--");
        command.args(files);
    }
    let status = command.status()?;
    if !status.success() {
        return Err(format!("git diff 失败（exit {}）", status.code().unwrap_or(-1)).into());
    }
    Ok(())
}

fn changed_files(job: &Job) -> Option<&[String]> {
    let files = job.result.as_ref()?.changed_files.as_slice();
    (!files.is_empty()).then_some(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Worker, WorkerRole, WorkerStatus};
    use crate::types::{JobStatus, NotificationMode, Session};
    use chrono::Utc;
    use std::collections::HashMap;

    fn ts() -> chrono::DateTime<Utc> {
        Utc::now()
    }

    fn worker() -> Worker {
        Worker {
            id: "w1".to_string(),
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
        }
    }

    fn session(git_strategy: GitStrategy, branch_start_commit: Option<&str>) -> Session {
        Session {
            id: "sess_1".to_string(),
            workspace_path: ".".to_string(),
            workspace_hash: "abc".to_string(),
            manager_id: None,
            workers: [("w1".to_string(), worker())].into_iter().collect(),
            jobs: [(
                "job_001".to_string(),
                Job {
                    id: "job_001".to_string(),
                    intent: "demo".to_string(),
                    status: JobStatus::Completed,
                    worker_id: Some("w1".to_string()),
                    depends_on: vec![],
                    created_at: ts(),
                    updated_at: ts(),
                    branch: None,
                    branch_start_commit: branch_start_commit.map(|s| s.to_string()),
                    checkpoints: vec![],
                    result: None,
                    fail_count: 0,
                    last_fail_at: None,
                },
            )]
            .into_iter()
            .collect(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 1,
            request_seq: 0,
            git_strategy,
            available_providers: vec!["codex".to_string()],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: ts(),
        }
    }

    #[test]
    fn test_job_diff_no_git_strategy() {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        storage
            .save_session(&session(GitStrategy::None, Some("abc123")))
            .unwrap();
        assert!(run_job_diff(temp.path().to_path_buf(), "job_001".to_string()).is_ok());
    }

    #[test]
    fn test_job_diff_no_start_commit() {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        storage
            .save_session(&session(GitStrategy::Branch, None))
            .unwrap();
        assert!(run_job_diff(temp.path().to_path_buf(), "job_001".to_string()).is_ok());
    }

    #[test]
    fn test_job_diff_missing_job() {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        storage
            .save_session(&session(GitStrategy::Branch, Some("abc123")))
            .unwrap();
        assert!(run_job_diff(temp.path().to_path_buf(), "job_999".to_string()).is_err());
    }

    #[test]
    fn changed_files_returns_none_for_empty_result() {
        let job = Job {
            id: "job_001".to_string(),
            intent: "demo".to_string(),
            status: JobStatus::Completed,
            worker_id: None,
            depends_on: vec![],
            created_at: ts(),
            updated_at: ts(),
            branch: None,
            branch_start_commit: Some("abc123".to_string()),
            checkpoints: vec![],
            result: None,
            fail_count: 0,
            last_fail_at: None,
        };
        assert!(changed_files(&job).is_none());
    }
}
