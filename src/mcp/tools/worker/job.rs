use super::{
    caller_job_id, changed_files_for_job, ensure_worker_owns_job, validate_min_chars, worker_id,
};
use crate::mcp::dispatcher::{Dispatcher, Tool};
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::queues::{NotificationQueue, RequestAwaiterRegistry};
use crate::mcp::server::ConnectedClient;
use crate::mcp::tools::manager::{
    append_action_log, git_current_commit, load_session, parse_params, save_session, storage_error,
    to_value, worker_mut,
};
use crate::storage::Storage;
use crate::types::{
    CheckpointContent, CheckpointMeta, CheckpointSummary, GitStrategy, JobResult,
    JobStatusResponse, ManagerNotification, RequestStatus, WorkerRole, WorkerStatus,
};
use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};

pub fn register(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    notifications: Arc<Mutex<NotificationQueue>>,
    awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
) {
    dispatcher.register(Box::new(JobProgressTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
    )));
    dispatcher.register(Box::new(JobCompleteTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
        Arc::clone(&notifications),
    )));
    dispatcher.register(Box::new(JobFailTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
        Arc::clone(&notifications),
    )));
    dispatcher.register(Box::new(JobCancelledTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
    )));
    dispatcher.register(Box::new(JobCheckpointTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
    )));
    dispatcher.register(Box::new(JobRequestTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
        Arc::clone(&notifications),
        Arc::clone(&awaiters),
    )));
    dispatcher.register(Box::new(JobRequestStatusTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
    )));
    dispatcher.register(Box::new(WorkerJobStatusTool::new(storage, push)));
}

#[derive(Deserialize)]
struct JobProgressParams {
    job_id: String,
    note: String,
}

pub struct JobProgressTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl JobProgressTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for JobProgressTool {
    fn name(&self) -> &str {
        "job.progress"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobProgressParams>(params)?;
        let mut session = load_session(&self.storage)?;
        ensure_worker_owns_job(&session, caller, &params.job_id)?;
        let worker = worker_mut(&mut session, &worker_id(caller)?)?;
        worker.last_progress = Some(Utc::now());
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "job_id": params.job_id, "note": params.note }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct JobCompleteParams {
    job_id: String,
    result_summary: String,
}

pub struct JobCompleteTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    notifications: Arc<Mutex<NotificationQueue>>,
}

impl JobCompleteTool {
    pub fn new(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        notifications: Arc<Mutex<NotificationQueue>>,
    ) -> Self {
        Self {
            storage,
            push,
            notifications,
        }
    }
}

#[async_trait]
impl Tool for JobCompleteTool {
    fn name(&self) -> &str {
        "job.complete"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobCompleteParams>(params)?;
        let mut session = load_session(&self.storage)?;
        if session.jobs[&params.job_id].status == crate::types::JobStatus::Completed {
            return Ok(Value::Null);
        }
        validate_min_chars("result_summary", &params.result_summary, 20)?;
        ensure_worker_owns_job(&session, caller, &params.job_id)?;

        let worker_id = worker_id(caller)?;
        let changed_files = changed_files_for_job(&session, &session.jobs[&params.job_id])?;
        let result = JobResult {
            summary: params.result_summary.clone(),
            changed_files: changed_files.clone(),
            completed_at: Utc::now(),
            worker_id: worker_id.clone(),
        };
        self.storage
            .save_result(&params.job_id, &result)
            .map_err(storage_error)?;

        {
            let job = session
                .jobs
                .get_mut(&params.job_id)
                .ok_or_else(|| McpError::JobNotFound(params.job_id.clone()))?;
            job.status = crate::types::JobStatus::Completed;
            job.updated_at = Utc::now();
            job.result = Some(result.clone());
        }
        {
            let worker = session
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| McpError::WorkerNotFound(worker_id.clone()))?;
            worker.status = WorkerStatus::Idle;
            worker.job_id = None;
        }

        let unblocked = session
            .jobs
            .values()
            .filter(|job| {
                job.status == crate::types::JobStatus::Waiting
                    && job
                        .depends_on
                        .iter()
                        .all(|dep| session.jobs[dep].status == crate::types::JobStatus::Completed)
            })
            .map(|job| job.id.clone())
            .collect::<Vec<_>>();
        for job_id in &unblocked {
            let job = session
                .jobs
                .get_mut(job_id)
                .ok_or_else(|| McpError::Internal(format!("job {job_id} disappeared")))?;
            job.status = crate::types::JobStatus::Pending;
            job.updated_at = Utc::now();
        }

        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "job_id": params.job_id, "result_summary": params.result_summary }),
            None,
        )?;

        let mut notifications = self.notifications.lock().await;
        notifications.push(ManagerNotification::JobCompleted {
            job_id: params.job_id.clone(),
            worker_id: worker_id.clone(),
            summary: result.summary.clone(),
            changed_files: changed_files.clone(),
        });
        for job_id in unblocked {
            notifications.push(ManagerNotification::JobUnblocked { job_id });
        }
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct JobFailParams {
    job_id: String,
    reason: String,
}

pub struct JobFailTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    notifications: Arc<Mutex<NotificationQueue>>,
}

impl JobFailTool {
    pub fn new(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        notifications: Arc<Mutex<NotificationQueue>>,
    ) -> Self {
        Self {
            storage,
            push,
            notifications,
        }
    }
}

#[async_trait]
impl Tool for JobFailTool {
    fn name(&self) -> &str {
        "job.fail"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobFailParams>(params)?;
        let mut session = load_session(&self.storage)?;
        ensure_worker_owns_job(&session, caller, &params.job_id)?;
        let worker_id = worker_id(caller)?;
        {
            let job = session
                .jobs
                .get_mut(&params.job_id)
                .ok_or_else(|| McpError::JobNotFound(params.job_id.clone()))?;
            job.status = crate::types::JobStatus::Failed;
            job.fail_count += 1;
            job.last_fail_at = Some(Utc::now());
            job.updated_at = Utc::now();
        }
        {
            let worker = session
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| McpError::WorkerNotFound(worker_id.clone()))?;
            worker.status = WorkerStatus::Idle;
            worker.job_id = None;
        }
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "job_id": params.job_id, "reason": params.reason }),
            None,
        )?;
        self.notifications
            .lock()
            .await
            .push(ManagerNotification::JobFailed {
                job_id: params.job_id,
                worker_id,
                reason: params.reason,
            });
        Ok(Value::Null)
    }
}

pub struct JobCancelledTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl JobCancelledTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for JobCancelledTool {
    fn name(&self) -> &str {
        "job.cancelled"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, _params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let mut session = load_session(&self.storage)?;
        let current_job_id = caller_job_id(&session, caller)?;
        let worker_id = worker_id(caller)?;
        {
            let job = session
                .jobs
                .get_mut(&current_job_id)
                .ok_or_else(|| McpError::JobNotFound(current_job_id.clone()))?;
            job.status = crate::types::JobStatus::Cancelled;
            job.updated_at = Utc::now();
        }
        {
            let worker = session
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| McpError::WorkerNotFound(worker_id.clone()))?;
            worker.status = WorkerStatus::Idle;
            worker.job_id = None;
        }
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "job_id": current_job_id }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct JobCheckpointParams {
    job_id: String,
    summary: CheckpointSummary,
}

pub struct JobCheckpointTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl JobCheckpointTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for JobCheckpointTool {
    fn name(&self) -> &str {
        "job.checkpoint"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobCheckpointParams>(params)?;
        let mut session = load_session(&self.storage)?;
        ensure_worker_owns_job(&session, caller, &params.job_id)?;
        validate_min_chars("done", &params.summary.done, 20)?;
        validate_min_chars("abandoned", &params.summary.abandoned, 20)?;
        validate_min_chars("in_progress", &params.summary.in_progress, 20)?;
        validate_min_chars("remaining", &params.summary.remaining, 20)?;
        validate_min_chars("pitfalls", &params.summary.pitfalls, 20)?;

        let checkpoint_id = format!("ckpt_{}", Utc::now().format("%Y%m%dT%H%M%S%3f"));
        let created_at = Utc::now();
        let git_commit = if session.git_strategy != GitStrategy::None {
            super::run_git(&session.workspace_path, &["add", "-A"])?;
            let message = format!(
                "[kingdom checkpoint] {}: {}",
                params.job_id,
                params.summary.done.chars().take(50).collect::<String>()
            );
            super::run_git(&session.workspace_path, &["commit", "-m", &message])?;
            Some(git_current_commit(&session.workspace_path)?)
        } else {
            None
        };

        let content = CheckpointContent {
            id: checkpoint_id.clone(),
            job_id: params.job_id.clone(),
            created_at,
            done: params.summary.done.clone(),
            abandoned: params.summary.abandoned.clone(),
            in_progress: params.summary.in_progress.clone(),
            remaining: params.summary.remaining.clone(),
            pitfalls: params.summary.pitfalls.clone(),
            git_commit: git_commit.clone(),
        };
        self.storage
            .save_checkpoint(&content)
            .map_err(storage_error)?;
        let job = session.jobs.get_mut(&params.job_id).unwrap();
        job.checkpoints.push(CheckpointMeta {
            id: checkpoint_id,
            job_id: params.job_id.clone(),
            created_at,
            git_commit: git_commit.clone(),
        });
        job.updated_at = Utc::now();
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "job_id": params.job_id, "summary": params.summary }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct JobRequestParams {
    job_id: String,
    question: String,
    blocking: bool,
}

pub struct JobRequestTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    notifications: Arc<Mutex<NotificationQueue>>,
    awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
}

impl JobRequestTool {
    pub fn new(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        notifications: Arc<Mutex<NotificationQueue>>,
        awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
    ) -> Self {
        Self {
            storage,
            push,
            notifications,
            awaiters,
        }
    }
}

#[async_trait]
impl Tool for JobRequestTool {
    fn name(&self) -> &str {
        "job.request"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobRequestParams>(params)?;
        let mut session = load_session(&self.storage)?;
        ensure_worker_owns_job(&session, caller, &params.job_id)?;
        let worker_id = worker_id(caller)?;
        let request_id = format!("req_{:03}", session.request_seq + 1);
        session.request_seq += 1;
        session.pending_requests.insert(
            request_id.clone(),
            crate::types::PendingRequest {
                id: request_id.clone(),
                job_id: params.job_id.clone(),
                worker_id: worker_id.clone(),
                question: params.question.clone(),
                blocking: params.blocking,
                answer: None,
                answered: false,
                created_at: Utc::now(),
                answered_at: None,
            },
        );
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({
                "job_id": params.job_id,
                "question": params.question,
                "blocking": params.blocking
            }),
            Some(json!({ "request_id": request_id })),
        )?;
        self.notifications
            .lock()
            .await
            .push(ManagerNotification::WorkerRequest {
                job_id: params.job_id.clone(),
                request_id: request_id.clone(),
                question: params.question.clone(),
                blocking: params.blocking,
            });

        if !params.blocking {
            return Ok(json!({ "request_id": request_id }));
        }

        let rx = self.awaiters.lock().await.register(&request_id);
        match tokio::time::timeout(Duration::from_secs(300), rx).await {
            Ok(Ok(answer)) => Ok(json!({ "request_id": request_id, "answer": answer })),
            Ok(Err(_)) | Err(_) => Err(McpError::InvalidState {
                message: "REQUEST_TIMEOUT".to_string(),
            }),
        }
    }
}

#[derive(Deserialize)]
struct RequestIdParams {
    request_id: String,
}

pub struct JobRequestStatusTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl JobRequestStatusTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for JobRequestStatusTool {
    fn name(&self) -> &str {
        "job.request_status"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<RequestIdParams>(params)?;
        let session = load_session(&self.storage)?;
        let request = session
            .pending_requests
            .get(&params.request_id)
            .ok_or_else(|| McpError::ValidationFailed {
                field: "request_id".to_string(),
                reason: "not found".to_string(),
            })?;
        to_value(&RequestStatus {
            request_id: request.id.clone(),
            answered: request.answered,
            answer: request.answer.clone(),
        })
    }
}

#[derive(Deserialize)]
struct JobStatusParams {
    job_id: String,
}

pub struct WorkerJobStatusTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkerJobStatusTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkerJobStatusTool {
    fn name(&self) -> &str {
        "job.status"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobStatusParams>(params)?;
        let session = load_session(&self.storage)?;
        ensure_worker_owns_job(&session, caller, &params.job_id)?;
        let job = &session.jobs[&params.job_id];
        let last_progress = session.workers[&worker_id(caller)?].last_progress;
        to_value(&JobStatusResponse {
            id: job.id.clone(),
            status: job.status.clone(),
            worker_id: job.worker_id.clone(),
            checkpoint_count: job.checkpoints.len() as u32,
            last_progress,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::dispatcher::Tool;
    use crate::mcp::tools::manager::job::JobRespondTool;
    use crate::mcp::tools::manager::testsupport::{manager_caller, ts};
    use crate::mcp::tools::worker::testsupport::{init_git_repo, setup_worker};
    use crate::types::{GitStrategy, Job, JobStatus, ManagerNotification};
    use serde_json::json;
    use std::sync::Arc;

    fn long_text(prefix: &str) -> String {
        format!("{prefix} {}", "x".repeat(30))
    }

    #[tokio::test]
    async fn job_complete_is_idempotent() {
        let (_temp, storage, push, notifications, _health, _awaiters, caller) = setup_worker();
        let tool = JobCompleteTool::new(
            Arc::clone(&storage),
            Arc::clone(&push),
            Arc::clone(&notifications),
        );
        tool.call(
            json!({"job_id":"job_001","result_summary":long_text("done")}),
            &caller,
        )
        .await
        .unwrap();
        tool.call(
            json!({"job_id":"job_001","result_summary":long_text("done")}),
            &caller,
        )
        .await
        .unwrap();
        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].status, JobStatus::Completed);
    }

    #[tokio::test]
    async fn job_complete_short_summary_fails() {
        let (_temp, storage, push, notifications, _health, _awaiters, caller) = setup_worker();
        let tool = JobCompleteTool::new(storage, push, notifications);
        let err = tool
            .call(
                json!({"job_id":"job_001","result_summary":"too short"}),
                &caller,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::ValidationFailed { field, .. } if field == "result_summary")
        );
    }

    #[tokio::test]
    async fn job_complete_none_strategy_has_no_changed_files() {
        let (_temp, storage, push, notifications, _health, _awaiters, caller) = setup_worker();
        let tool = JobCompleteTool::new(Arc::clone(&storage), push, Arc::clone(&notifications));
        tool.call(
            json!({"job_id":"job_001","result_summary":long_text("done")}),
            &caller,
        )
        .await
        .unwrap();
        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(
            session.jobs["job_001"]
                .result
                .as_ref()
                .unwrap()
                .changed_files,
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn job_complete_branch_strategy_computes_changed_files_and_unblocks_job() {
        let (temp, storage, push, notifications, _health, _awaiters, caller) = setup_worker();
        init_git_repo(temp.path());
        std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
        super::super::run_git(temp.path().to_str().unwrap(), &["add", "-A"]).unwrap();
        super::super::run_git(temp.path().to_str().unwrap(), &["commit", "-m", "initial"]).unwrap();
        let base = git_current_commit(temp.path().to_str().unwrap()).unwrap();
        std::fs::write(temp.path().join("tracked.txt"), "base\nchange\n").unwrap();
        super::super::run_git(temp.path().to_str().unwrap(), &["add", "-A"]).unwrap();
        super::super::run_git(temp.path().to_str().unwrap(), &["commit", "-m", "update"]).unwrap();

        let mut session = storage.load_session().unwrap().unwrap();
        session.git_strategy = GitStrategy::Branch;
        session.jobs.get_mut("job_001").unwrap().branch_start_commit = Some(base);
        session.jobs.insert(
            "job_002".to_string(),
            Job {
                id: "job_002".to_string(),
                intent: "Follow-up".to_string(),
                status: JobStatus::Waiting,
                worker_id: None,
                depends_on: vec!["job_001".to_string()],
                created_at: ts(),
                updated_at: ts(),
                branch: None,
                branch_start_commit: None,
                checkpoints: vec![],
                result: None,
                fail_count: 0,
                last_fail_at: None,
            },
        );
        storage.save_session(&session).unwrap();

        let tool = JobCompleteTool::new(Arc::clone(&storage), push, Arc::clone(&notifications));
        tool.call(
            json!({"job_id":"job_001","result_summary":long_text("done")}),
            &caller,
        )
        .await
        .unwrap();
        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(
            session.jobs["job_001"]
                .result
                .as_ref()
                .unwrap()
                .changed_files,
            vec!["tracked.txt".to_string()]
        );
        assert_eq!(session.jobs["job_002"].status, JobStatus::Pending);
        let events = notifications.lock().await.drain();
        assert!(events.iter().any(|event| matches!(event, ManagerNotification::JobUnblocked { job_id } if job_id == "job_002")));
    }

    #[tokio::test]
    async fn job_fail_sets_status_and_queues_notification() {
        let (_temp, storage, push, notifications, _health, _awaiters, caller) = setup_worker();
        let tool = JobFailTool::new(Arc::clone(&storage), push, Arc::clone(&notifications));
        tool.call(
            json!({"job_id":"job_001","reason":"failure reason long enough"}),
            &caller,
        )
        .await
        .unwrap();
        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].status, JobStatus::Failed);
        let events = notifications.lock().await.drain();
        assert!(events.iter().any(|event| matches!(event, ManagerNotification::JobFailed { job_id, .. } if job_id == "job_001")));
    }

    #[tokio::test]
    async fn job_cancelled_sets_cancelled_not_failed() {
        let (_temp, storage, push, _notifications, _health, _awaiters, caller) = setup_worker();
        let tool = JobCancelledTool::new(Arc::clone(&storage), push);
        tool.call(Value::Null, &caller).await.unwrap();
        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].status, JobStatus::Cancelled);
    }

    #[tokio::test]
    async fn job_checkpoint_validates_min_length() {
        let (_temp, storage, push, _notifications, _health, _awaiters, caller) = setup_worker();
        let tool = JobCheckpointTool::new(storage, push);
        let err = tool
            .call(
                json!({"job_id":"job_001","summary":{
                    "done":"short",
                    "abandoned":long_text("abandoned"),
                    "in_progress":long_text("in progress"),
                    "remaining":long_text("remaining"),
                    "pitfalls":long_text("pitfalls")
                }}),
                &caller,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ValidationFailed { field, .. } if field == "done"));
    }

    #[tokio::test]
    async fn job_checkpoint_none_strategy_succeeds_without_git_commit() {
        let (_temp, storage, push, _notifications, _health, _awaiters, caller) = setup_worker();
        let tool = JobCheckpointTool::new(Arc::clone(&storage), push);
        tool.call(
            json!({"job_id":"job_001","summary":{
                "done":long_text("done"),
                "abandoned":long_text("abandoned"),
                "in_progress":long_text("in progress"),
                "remaining":long_text("remaining"),
                "pitfalls":long_text("pitfalls")
            }}),
            &caller,
        )
        .await
        .unwrap();
        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].checkpoints.len(), 1);
        assert!(session.jobs["job_001"].checkpoints[0].git_commit.is_none());
    }

    #[tokio::test]
    async fn job_checkpoint_branch_strategy_creates_commit() {
        let (temp, storage, push, _notifications, _health, _awaiters, caller) = setup_worker();
        init_git_repo(temp.path());
        std::fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
        super::super::run_git(temp.path().to_str().unwrap(), &["add", "-A"]).unwrap();
        super::super::run_git(temp.path().to_str().unwrap(), &["commit", "-m", "initial"]).unwrap();
        std::fs::write(temp.path().join("tracked.txt"), "base\nnext\n").unwrap();
        let mut session = storage.load_session().unwrap().unwrap();
        session.git_strategy = GitStrategy::Branch;
        storage.save_session(&session).unwrap();

        let tool = JobCheckpointTool::new(Arc::clone(&storage), push);
        tool.call(
            json!({"job_id":"job_001","summary":{
                "done":long_text("done"),
                "abandoned":long_text("abandoned"),
                "in_progress":long_text("in progress"),
                "remaining":long_text("remaining"),
                "pitfalls":long_text("pitfalls")
            }}),
            &caller,
        )
        .await
        .unwrap();
        let session = storage.load_session().unwrap().unwrap();
        assert!(session.jobs["job_001"].checkpoints[0].git_commit.is_some());
    }

    #[tokio::test]
    async fn job_request_non_blocking_returns_request_id() {
        let (_temp, storage, push, notifications, _health, awaiters, caller) = setup_worker();
        let tool = JobRequestTool::new(storage, push, notifications, awaiters);
        let value = tool
            .call(
                json!({"job_id":"job_001","question":"question long enough","blocking":false}),
                &caller,
            )
            .await
            .unwrap();
        assert!(value["request_id"].as_str().unwrap().starts_with("req_"));
    }

    #[tokio::test]
    async fn job_request_blocking_waits_for_job_respond_signal() {
        let (_temp, storage, push, notifications, _health, awaiters, caller) = setup_worker();
        let request_tool = JobRequestTool::new(
            Arc::clone(&storage),
            Arc::clone(&push),
            Arc::clone(&notifications),
            Arc::clone(&awaiters),
        );
        let respond_tool = JobRespondTool::new(
            Arc::clone(&storage),
            Arc::clone(&push),
            Arc::clone(&awaiters),
        );

        let worker_handle = tokio::spawn(async move {
            request_tool
                .call(json!({"job_id":"job_001","question":"blocking question long enough","blocking":true}), &caller)
                .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let request_id = {
            let session = storage.load_session().unwrap().unwrap();
            session.pending_requests.keys().next().unwrap().clone()
        };
        respond_tool
            .call(
                json!({"request_id":request_id,"answer":"manager answer"}),
                &manager_caller(),
            )
            .await
            .unwrap();

        let result = worker_handle.await.unwrap().unwrap();
        assert_eq!(result["answer"], "manager answer");
    }

    #[tokio::test]
    async fn worker_job_status_checks_ownership() {
        let (_temp, storage, push, _notifications, _health, _awaiters, caller) = setup_worker();
        let tool = WorkerJobStatusTool::new(Arc::clone(&storage), push);
        let ok = tool
            .call(json!({"job_id":"job_001"}), &caller)
            .await
            .unwrap();
        assert_eq!(ok["id"], "job_001");
        let err = tool
            .call(json!({"job_id":"job_999"}), &caller)
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Unauthorized { tool, .. } if tool == "job.status"));
    }
}
