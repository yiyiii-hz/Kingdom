use crate::mcp::dispatcher::Dispatcher;
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::queues::{HealthEventQueue, NotificationQueue, RequestAwaiterRegistry};
use crate::mcp::server::ConnectedClient;
use crate::mcp::tools::manager::{git_create_branch, git_current_commit, storage_error};
use crate::storage::Storage;
use crate::types::{CheckpointUrgency, GitStrategy, Job, JobStatus, Session};
use chrono::Utc;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub mod context;
pub mod file;
pub mod git;
pub mod job;
pub mod subtask;

pub fn register(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    notifications: Arc<Mutex<NotificationQueue>>,
    health_events: Arc<Mutex<HealthEventQueue>>,
    awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
) {
    job::register(
        dispatcher,
        Arc::clone(&storage),
        Arc::clone(&push),
        Arc::clone(&notifications),
        Arc::clone(&awaiters),
    );
    context::register(
        dispatcher,
        Arc::clone(&storage),
        Arc::clone(&push),
        Arc::clone(&health_events),
    );
    file::register(dispatcher, Arc::clone(&storage), Arc::clone(&push));
    git::register(dispatcher, Arc::clone(&storage), Arc::clone(&push));
    subtask::register(dispatcher, storage, push, notifications);
}

pub(crate) fn caller_job_id(
    session: &Session,
    caller: &ConnectedClient,
) -> Result<String, McpError> {
    let worker_id = caller
        .worker_id
        .as_deref()
        .ok_or_else(|| McpError::Unauthorized {
            tool: "worker-tool".to_string(),
            role: "no worker_id".to_string(),
        })?;
    let worker = session
        .workers
        .get(worker_id)
        .ok_or_else(|| McpError::WorkerNotFound(worker_id.to_string()))?;
    worker.job_id.clone().ok_or_else(|| McpError::InvalidState {
        message: "WORKER_HAS_NO_JOB".to_string(),
    })
}

pub(crate) fn ensure_worker_owns_job(
    session: &Session,
    caller: &ConnectedClient,
    job_id: &str,
) -> Result<String, McpError> {
    let current_job_id = caller_job_id(session, caller)?;
    if current_job_id != job_id {
        return Err(McpError::Unauthorized {
            tool: "job.status".to_string(),
            role: "worker".to_string(),
        });
    }
    Ok(current_job_id)
}

pub(crate) fn worker_id(caller: &ConnectedClient) -> Result<String, McpError> {
    caller
        .worker_id
        .clone()
        .ok_or_else(|| McpError::Unauthorized {
            tool: "worker-tool".to_string(),
            role: "no worker_id".to_string(),
        })
}

pub(crate) fn validate_min_chars(field: &str, value: &str, min: usize) -> Result<(), McpError> {
    if value.trim().is_empty() || value.chars().count() < min {
        return Err(McpError::ValidationFailed {
            field: field.to_string(),
            reason: format!("must be at least {min} characters"),
        });
    }
    Ok(())
}

pub(crate) fn changed_files_for_job(session: &Session, job: &Job) -> Result<Vec<String>, McpError> {
    if !matches!(
        session.git_strategy,
        GitStrategy::Branch | GitStrategy::Commit
    ) {
        return Ok(Vec::new());
    }
    let Some(base) = &job.branch_start_commit else {
        return Ok(Vec::new());
    };
    let output = Command::new("git")
        .args([
            "-C",
            &session.workspace_path,
            "diff",
            "--name-only",
            &format!("{base}..HEAD"),
        ])
        .output()
        .map_err(storage_error)?;
    if !output.status.success() {
        return Err(storage_error(
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(ToString::to_string)
        .collect())
}

pub(crate) fn run_git(workspace_path: &str, args: &[&str]) -> Result<String, McpError> {
    let output = Command::new("git")
        .args(["-C", workspace_path])
        .args(args)
        .output()
        .map_err(storage_error)?;
    if !output.status.success() {
        return Err(storage_error(
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(crate) fn context_urgency(usage_pct: f32) -> Option<CheckpointUrgency> {
    if usage_pct >= 0.85 {
        Some(CheckpointUrgency::Critical)
    } else if usage_pct >= 0.70 {
        Some(CheckpointUrgency::High)
    } else if usage_pct >= 0.50 {
        Some(CheckpointUrgency::Normal)
    } else {
        None
    }
}

pub(crate) fn resolve_workspace_path(
    workspace_root: &str,
    requested_path: &str,
) -> Result<PathBuf, McpError> {
    if Path::new(requested_path)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(McpError::ValidationFailed {
            field: "path".to_string(),
            reason: "path traversal not allowed".to_string(),
        });
    }
    let workspace = Path::new(workspace_root)
        .canonicalize()
        .map_err(storage_error)?;
    let joined = workspace.join(requested_path);
    let resolved = if joined.exists() {
        joined.canonicalize().map_err(storage_error)?
    } else {
        let parent = joined
            .parent()
            .ok_or_else(|| McpError::ValidationFailed {
                field: "path".to_string(),
                reason: "path traversal not allowed".to_string(),
            })?
            .canonicalize()
            .map_err(storage_error)?;
        let file_name = joined
            .file_name()
            .ok_or_else(|| McpError::ValidationFailed {
                field: "path".to_string(),
                reason: "path traversal not allowed".to_string(),
            })?;
        parent.join(file_name)
    };
    if !resolved.starts_with(&workspace) {
        return Err(McpError::ValidationFailed {
            field: "path".to_string(),
            reason: "path traversal not allowed".to_string(),
        });
    }
    Ok(resolved)
}

pub(crate) fn default_tree_path(
    workspace_root: &str,
    path: Option<String>,
) -> Result<PathBuf, McpError> {
    match path {
        Some(path) => resolve_workspace_path(workspace_root, &path),
        None => Path::new(workspace_root)
            .canonicalize()
            .map_err(storage_error),
    }
}

pub(crate) fn create_job(
    session: &mut Session,
    intent: String,
    mut depends_on: Vec<String>,
) -> Result<String, McpError> {
    for dep in &depends_on {
        if !session.jobs.contains_key(dep) {
            return Err(McpError::JobNotFound(dep.clone()));
        }
    }

    let job_id = format!("job_{:03}", session.job_seq + 1);
    let now = Utc::now();
    let status = if depends_on.is_empty()
        || depends_on
            .iter()
            .all(|dep| session.jobs[dep].status == JobStatus::Completed)
    {
        JobStatus::Pending
    } else {
        JobStatus::Waiting
    };
    let (branch, branch_start_commit) = match session.git_strategy {
        GitStrategy::Branch => {
            let branch = format!("kingdom/{job_id}");
            git_create_branch(&session.workspace_path, &branch)?;
            let commit = git_current_commit(&session.workspace_path)?;
            (Some(branch), Some(commit))
        }
        GitStrategy::Commit => (None, Some(git_current_commit(&session.workspace_path)?)),
        GitStrategy::None => (None, None),
    };
    session.job_seq += 1;
    session.jobs.insert(
        job_id.clone(),
        Job {
            id: job_id.clone(),
            intent,
            status,
            worker_id: None,
            depends_on: std::mem::take(&mut depends_on),
            created_at: now,
            updated_at: now,
            branch,
            branch_start_commit,
            checkpoints: vec![],
            result: None,
            fail_count: 0,
            last_fail_at: None,
        },
    );
    Ok(job_id)
}

#[cfg(test)]
pub(crate) mod testsupport {
    use super::*;
    use crate::mcp::push::PushRegistry;
    use crate::mcp::queues::{HealthEventQueue, NotificationQueue, RequestAwaiterRegistry};
    use crate::mcp::tools::manager::testsupport::{session_with_workspace, ts};
    use crate::types::{Permission, Worker, WorkerRole, WorkerStatus};
    use tempfile::{tempdir, TempDir};

    pub(crate) fn worker_caller(worker_id: &str) -> ConnectedClient {
        ConnectedClient {
            connection_id: format!("conn_{worker_id}"),
            worker_id: Some(worker_id.to_string()),
            role: WorkerRole::Worker,
            session_id: "sess_test".to_string(),
        }
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn setup_worker() -> (
        TempDir,
        Arc<Storage>,
        Arc<RwLock<PushRegistry>>,
        Arc<Mutex<NotificationQueue>>,
        Arc<Mutex<HealthEventQueue>>,
        Arc<Mutex<RequestAwaiterRegistry>>,
        ConnectedClient,
    ) {
        let temp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(temp.path()).unwrap());
        let mut session = session_with_workspace(temp.path().to_str().unwrap());
        let worker = Worker {
            id: "w1".to_string(),
            provider: "codex".to_string(),
            role: WorkerRole::Worker,
            status: WorkerStatus::Running,
            job_id: Some("job_001".to_string()),
            pid: None,
            pane_id: String::new(),
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
            intent: "Implement worker tools".to_string(),
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
        };
        session.workers.insert(worker.id.clone(), worker.clone());
        session.jobs.insert(job.id.clone(), job);
        storage.save_session(&session).unwrap();
        (
            temp,
            storage,
            Arc::new(RwLock::new(PushRegistry::new())),
            Arc::new(Mutex::new(NotificationQueue::new())),
            Arc::new(Mutex::new(HealthEventQueue::new())),
            Arc::new(Mutex::new(RequestAwaiterRegistry::new())),
            worker_caller("w1"),
        )
    }

    pub(crate) fn grant_permission(storage: &Storage, worker_id: &str, permission: Permission) {
        let mut session = storage.load_session().unwrap().unwrap();
        session
            .workers
            .get_mut(worker_id)
            .unwrap()
            .permissions
            .push(permission);
        storage.save_session(&session).unwrap();
    }

    pub(crate) fn init_git_repo(path: &Path) {
        let path = path.to_str().unwrap();
        run_git(path, &["init"]).unwrap();
        run_git(path, &["config", "user.email", "test@example.com"]).unwrap();
        run_git(path, &["config", "user.name", "Test User"]).unwrap();
    }
}
