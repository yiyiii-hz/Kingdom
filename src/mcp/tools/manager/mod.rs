use crate::mcp::dispatcher::Dispatcher;
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::queues::{HealthEventQueue, NotificationQueue, RequestAwaiterRegistry};
use crate::mcp::server::ConnectedClient;
use crate::storage::Storage;
use crate::types::{
    ActionLogEntry, Job, JobStatus, NoteScope, Permission, Session, Worker, WorkerStatus,
};
use chrono::Utc;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub mod failover;
pub mod job;
pub mod worker;
pub mod workspace;

pub fn register(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    notifications: Arc<Mutex<NotificationQueue>>,
    health_events: Arc<Mutex<HealthEventQueue>>,
    awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
) {
    let _ = (&notifications, &health_events);
    workspace::register(dispatcher, Arc::clone(&storage), Arc::clone(&push));
    worker::register(dispatcher, Arc::clone(&storage), Arc::clone(&push));
    job::register(
        dispatcher,
        Arc::clone(&storage),
        Arc::clone(&push),
        Arc::clone(&awaiters),
    );
    failover::register(dispatcher, storage, push);
}

pub(crate) fn load_session(storage: &Storage) -> Result<Session, McpError> {
    storage
        .load_session()
        .map_err(storage_error)?
        .ok_or_else(|| McpError::InvalidState {
            message: "SESSION_NOT_INITIALIZED".to_string(),
        })
}

pub(crate) fn save_session(storage: &Storage, session: &Session) -> Result<(), McpError> {
    storage.save_session(session).map_err(storage_error)
}

pub(crate) fn storage_error(error: impl std::fmt::Display) -> McpError {
    McpError::Internal(error.to_string())
}

pub(crate) fn parse_params<T: DeserializeOwned>(params: Value) -> Result<T, McpError> {
    serde_json::from_value(params).map_err(|error| McpError::ValidationFailed {
        field: "params".to_string(),
        reason: error.to_string(),
    })
}

pub(crate) fn to_value<T: Serialize>(value: &T) -> Result<Value, McpError> {
    serde_json::to_value(value).map_err(|error| McpError::Internal(error.to_string()))
}

pub(crate) fn action_actor(caller: &ConnectedClient) -> String {
    caller
        .worker_id
        .clone()
        .unwrap_or_else(|| "manager".to_string())
}

pub(crate) fn append_action_log(
    storage: &Storage,
    caller: &ConnectedClient,
    action: &str,
    params: Value,
    result: Option<Value>,
) -> Result<(), McpError> {
    let entry = ActionLogEntry {
        timestamp: Utc::now(),
        actor: action_actor(caller),
        action: action.to_string(),
        params,
        result,
        error: None,
    };
    storage.append_action_log(&entry).map_err(storage_error)
}

pub(crate) fn parse_scope(scope: Option<String>) -> NoteScope {
    match scope {
        None => NoteScope::Global,
        Some(scope) if scope == "global" => NoteScope::Global,
        Some(scope) if scope.starts_with("job:") => {
            NoteScope::Job(scope.trim_start_matches("job:").to_string())
        }
        Some(scope) => NoteScope::Directory(scope),
    }
}

pub(crate) fn permission_from_str(permission: &str) -> Result<Permission, McpError> {
    match permission {
        "subtask_create" => Ok(Permission::SubtaskCreate),
        "worker_notify" => Ok(Permission::WorkerNotify),
        "workspace_read" => Ok(Permission::WorkspaceRead),
        "job_read_all" => Ok(Permission::JobReadAll),
        _ => Err(McpError::ValidationFailed {
            field: "permission".to_string(),
            reason: "unknown permission".to_string(),
        }),
    }
}

pub(crate) fn worker_mut<'a>(
    session: &'a mut Session,
    worker_id: &str,
) -> Result<&'a mut Worker, McpError> {
    session
        .workers
        .get_mut(worker_id)
        .ok_or_else(|| McpError::WorkerNotFound(worker_id.to_string()))
}

pub(crate) fn worker_ref<'a>(session: &'a Session, worker_id: &str) -> Result<&'a Worker, McpError> {
    session
        .workers
        .get(worker_id)
        .ok_or_else(|| McpError::WorkerNotFound(worker_id.to_string()))
}

pub(crate) fn job_mut<'a>(session: &'a mut Session, job_id: &str) -> Result<&'a mut Job, McpError> {
    session
        .jobs
        .get_mut(job_id)
        .ok_or_else(|| McpError::JobNotFound(job_id.to_string()))
}

pub(crate) fn job_ref<'a>(session: &'a Session, job_id: &str) -> Result<&'a Job, McpError> {
    session
        .jobs
        .get(job_id)
        .ok_or_else(|| McpError::JobNotFound(job_id.to_string()))
}

pub(crate) fn assign_job(session: &mut Session, worker_id: &str, job_id: &str) -> Result<(), McpError> {
    let worker_status = worker_ref(session, worker_id)?.status.clone();
    match worker_status {
        WorkerStatus::Idle => {}
        WorkerStatus::Running => {
            return Err(McpError::InvalidState {
                message: "WORKER_BUSY".to_string(),
            });
        }
        WorkerStatus::Starting | WorkerStatus::Terminated => {
            return Err(McpError::InvalidState {
                message: "WORKER_NOT_FOUND".to_string(),
            });
        }
        WorkerStatus::Failed => {
            return Err(McpError::InvalidState {
                message: "WORKER_NOT_AVAILABLE".to_string(),
            });
        }
    }

    let job_status = job_ref(session, job_id)?.status.clone();
    if job_status != JobStatus::Pending {
        return Err(McpError::InvalidState {
            message: "JOB_NOT_PENDING".to_string(),
        });
    }

    let now = Utc::now();
    {
        let job = job_mut(session, job_id)?;
        job.worker_id = Some(worker_id.to_string());
        job.status = JobStatus::Running;
        job.updated_at = now;
    }
    {
        let worker = worker_mut(session, worker_id)?;
        worker.job_id = Some(job_id.to_string());
        worker.status = WorkerStatus::Running;
    }
    Ok(())
}

pub(crate) fn sort_notes(notes: &mut [crate::types::WorkspaceNote]) {
    notes.sort_by(|a, b| note_scope_rank(&a.scope).cmp(&note_scope_rank(&b.scope)));
}

fn note_scope_rank(scope: &NoteScope) -> u8 {
    match scope {
        NoteScope::Job(_) => 0,
        NoteScope::Directory(_) => 1,
        NoteScope::Global => 2,
    }
}

pub(crate) fn load_result(storage: &Storage, job_id: &str) -> Result<crate::types::JobResult, McpError> {
    let path: PathBuf = storage.root.join("jobs").join(job_id).join("result.json");
    let bytes = std::fs::read(path).map_err(storage_error)?;
    serde_json::from_slice(&bytes).map_err(|error| McpError::Internal(error.to_string()))
}

pub(crate) fn git_current_commit(workspace_path: &str) -> Result<String, McpError> {
    let output = Command::new("git")
        .args(["-C", workspace_path, "rev-parse", "HEAD"])
        .output()
        .map_err(storage_error)?;
    if !output.status.success() {
        return Err(McpError::Internal(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn git_create_branch(workspace_path: &str, branch: &str) -> Result<(), McpError> {
    let output = Command::new("git")
        .args(["-C", workspace_path, "checkout", "-b", branch])
        .output()
        .map_err(storage_error)?;
    if !output.status.success() {
        return Err(McpError::Internal(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testsupport {
    use super::*;
    use crate::mcp::dispatcher::Tool;
    use crate::mcp::server::ConnectedClient;
    use crate::types::{
        FailoverReason, GitStrategy, HandoffBrief, JobResult, NotificationMode, PendingFailover,
        PendingFailoverStatus, PendingRequest, Session, Worker, WorkerRole, WorkerStatus,
    };
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;
    use tempfile::{tempdir, TempDir};

    pub(crate) fn ts() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 6, 12, 0, 0).unwrap()
    }

    pub(crate) fn manager_caller() -> ConnectedClient {
        ConnectedClient {
            connection_id: "conn_manager".to_string(),
            worker_id: None,
            role: WorkerRole::Manager,
            session_id: "sess_test".to_string(),
        }
    }

    pub(crate) fn worker(worker_id: &str, status: WorkerStatus) -> Worker {
        Worker {
            id: worker_id.to_string(),
            provider: "codex".to_string(),
            role: WorkerRole::Worker,
            status,
            job_id: None,
            pid: None,
            pane_id: String::new(),
            mcp_connected: false,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: None,
            last_progress: None,
            permissions: vec![],
            started_at: ts(),
        }
    }

    pub(crate) fn manager_worker(worker_id: &str) -> Worker {
        Worker {
            role: WorkerRole::Manager,
            ..worker(worker_id, WorkerStatus::Idle)
        }
    }

    pub(crate) fn session_with_workspace(workspace_path: &str) -> Session {
        let manager = manager_worker("wm");
        Session {
            id: "sess_test".to_string(),
            workspace_path: workspace_path.to_string(),
            workspace_hash: "hash".to_string(),
            manager_id: Some(manager.id.clone()),
            workers: [(manager.id.clone(), manager)].into_iter().collect::<HashMap<_, _>>(),
            jobs: HashMap::new(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: GitStrategy::None,
            available_providers: vec!["codex".to_string(), "gemini".to_string()],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            created_at: ts(),
        }
    }

    pub(crate) fn setup() -> (TempDir, Arc<Storage>, Arc<RwLock<PushRegistry>>, ConnectedClient) {
        let temp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(temp.path()).unwrap());
        let session = session_with_workspace(temp.path().to_str().unwrap());
        storage.save_session(&session).unwrap();
        (
            temp,
            storage,
            Arc::new(RwLock::new(PushRegistry::new())),
            manager_caller(),
        )
    }

    pub(crate) async fn call_tool(
        tool: &dyn Tool,
        params: Value,
        caller: &ConnectedClient,
    ) -> Result<Value, McpError> {
        tool.call(params, caller).await
    }

    pub(crate) fn sample_pending_request() -> PendingRequest {
        PendingRequest {
            id: "req_001".to_string(),
            job_id: "job_001".to_string(),
            worker_id: "w1".to_string(),
            question: "Need clarification".to_string(),
            blocking: true,
            answer: None,
            answered: false,
            created_at: ts(),
            answered_at: None,
        }
    }

    pub(crate) fn sample_pending_failover() -> PendingFailover {
        PendingFailover {
            worker_id: "w1".to_string(),
            job_id: "job_001".to_string(),
            reason: FailoverReason::ContextLimit,
            handoff_brief: HandoffBrief {
                job_id: "job_001".to_string(),
                original_intent: "Fix bug".to_string(),
                done: "Inspected issue".to_string(),
                in_progress: "Preparing patch".to_string(),
                remaining: "Apply patch".to_string(),
                pitfalls: "Keep migration safe".to_string(),
                possibly_incomplete_files: vec!["src/lib.rs".to_string()],
                changed_files: vec!["src/main.rs".to_string()],
            },
            recommended_provider: Some("gemini".to_string()),
            created_at: ts(),
            status: PendingFailoverStatus::WaitingConfirmation,
        }
    }

    pub(crate) fn sample_result() -> JobResult {
        JobResult {
            summary: "Implemented feature".to_string(),
            changed_files: vec!["src/lib.rs".to_string()],
            completed_at: ts(),
            worker_id: "w1".to_string(),
        }
    }
}
