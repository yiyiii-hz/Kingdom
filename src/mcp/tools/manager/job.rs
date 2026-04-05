use super::{
    append_action_log, assign_job, git_create_branch, git_current_commit, load_result,
    load_session, parse_params, save_session, to_value,
};
use crate::mcp::dispatcher::{Dispatcher, Tool};
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::queues::RequestAwaiterRegistry;
use crate::mcp::server::ConnectedClient;
use crate::storage::Storage;
use crate::types::{
    GitStrategy, Job, JobResultResponse, JobStatus, JobStatusResponse, WorkerRole, WorkerStatus,
};
use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub fn register(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
) {
    dispatcher.register(Box::new(JobCreateTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(JobStatusTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(JobResultTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(JobCancelTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(JobKeepWaitingTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(JobUpdateTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(JobRespondTool::new(storage, push, awaiters)));
}

#[derive(Deserialize)]
struct JobCreateParams {
    intent: String,
    worker_id: Option<String>,
    depends_on: Option<Vec<String>>,
}

pub struct JobCreateTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl JobCreateTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for JobCreateTool {
    fn name(&self) -> &str {
        "job.create"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobCreateParams>(params.clone())?;
        let mut session = load_session(&self.storage)?;
        let job_id = format!("job_{:03}", session.job_seq + 1);
        let depends_on = params.depends_on.unwrap_or_default();

        for dep in &depends_on {
            if !session.jobs.contains_key(dep) {
                return Err(McpError::JobNotFound(dep.clone()));
            }
        }

        let initial_status = if depends_on.is_empty()
            || depends_on
                .iter()
                .all(|dep| session.jobs[dep].status == JobStatus::Completed)
        {
            JobStatus::Pending
        } else {
            JobStatus::Waiting
        };

        let now = Utc::now();
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

        let job = Job {
            id: job_id.clone(),
            intent: params.intent.clone(),
            status: initial_status,
            worker_id: None,
            depends_on,
            created_at: now,
            updated_at: now,
            branch,
            branch_start_commit,
            checkpoints: vec![],
            result: None,
            fail_count: 0,
            last_fail_at: None,
        };
        session.job_seq += 1;
        session.jobs.insert(job_id.clone(), job);

        if let Some(worker_id) = &params.worker_id {
            assign_job(&mut session, worker_id, &job_id)?;
        }

        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({
                "intent": params.intent,
                "worker_id": params.worker_id,
                "depends_on": session.jobs.get(&job_id).map(|job| job.depends_on.clone()).unwrap_or_default()
            }),
            Some(json!({ "job_id": job_id })),
        )?;
        Ok(Value::String(job_id))
    }
}

#[derive(Deserialize)]
struct JobIdParams {
    job_id: String,
}

pub struct JobStatusTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl JobStatusTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for JobStatusTool {
    fn name(&self) -> &str {
        "job.status"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager, WorkerRole::Worker]
    }

    async fn call(&self, params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobIdParams>(params)?;
        let session = load_session(&self.storage)?;
        let job = session
            .jobs
            .get(&params.job_id)
            .ok_or_else(|| McpError::JobNotFound(params.job_id.clone()))?;
        let last_progress = job
            .worker_id
            .as_ref()
            .and_then(|worker_id| session.workers.get(worker_id))
            .and_then(|worker| worker.last_progress);
        to_value(&JobStatusResponse {
            id: job.id.clone(),
            status: job.status.clone(),
            worker_id: job.worker_id.clone(),
            checkpoint_count: job.checkpoints.len() as u32,
            last_progress,
        })
    }
}

pub struct JobResultTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl JobResultTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for JobResultTool {
    fn name(&self) -> &str {
        "job.result"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobIdParams>(params)?;
        let session = load_session(&self.storage)?;
        let job = session
            .jobs
            .get(&params.job_id)
            .ok_or_else(|| McpError::JobNotFound(params.job_id.clone()))?;
        if job.status != JobStatus::Completed {
            return Err(McpError::InvalidState {
                message: "JOB_NOT_COMPLETED".to_string(),
            });
        }
        let result = load_result(&self.storage, &params.job_id)?;
        to_value(&JobResultResponse {
            id: job.id.clone(),
            summary: result.summary,
            changed_files: result.changed_files,
            checkpoint_count: job.checkpoints.len() as u32,
            branch: job.branch.clone(),
            completed_at: result.completed_at,
        })
    }
}

pub struct JobCancelTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl JobCancelTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for JobCancelTool {
    fn name(&self) -> &str {
        "job.cancel"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let params = parse_params::<JobIdParams>(params.clone())?;
        let mut session = load_session(&self.storage)?;
        let job = session
            .jobs
            .get(&params.job_id)
            .cloned()
            .ok_or_else(|| McpError::JobNotFound(params.job_id.clone()))?;

        let cascade = session
            .jobs
            .values()
            .filter(|candidate| {
                candidate.status == JobStatus::Waiting
                    && candidate.depends_on.iter().any(|dep| dep == &params.job_id)
            })
            .map(|job| job.id.clone())
            .collect::<Vec<_>>();

        if !cascade.is_empty() {
            append_action_log(
                &self.storage,
                caller,
                self.name(),
                json!({
                    "job_id": params.job_id,
                    "warning": "cancel_cascade",
                    "affected_jobs": cascade,
                }),
                None,
            )?;
        }

        let should_push = job
            .worker_id
            .as_ref()
            .and_then(|worker_id| session.workers.get(worker_id))
            .is_some_and(|worker| worker.status == WorkerStatus::Running);

        if should_push {
            let job_mut = session.jobs.get_mut(&job.id).unwrap();
            job_mut.status = JobStatus::Cancelling;
            job_mut.updated_at = Utc::now();
            save_session(&self.storage, &session)?;

            if let Some(worker_id) = &job.worker_id {
                self.push
                    .read()
                    .await
                    .push(
                        worker_id,
                        json!({
                            "jsonrpc":"2.0",
                            "method":"kingdom.cancel_job",
                            "params":{"job_id":job.id,"reason":"manager_cancelled"}
                        }),
                    )
                    .await
                    .map_err(super::storage_error)?;
            }
        } else {
            if let Some(job_mut) = session.jobs.get_mut(&job.id) {
                job_mut.status = JobStatus::Cancelled;
                job_mut.updated_at = Utc::now();
            }
            if let Some(worker_id) = &job.worker_id {
                if let Some(worker) = session.workers.get_mut(worker_id) {
                    worker.status = WorkerStatus::Idle;
                    worker.job_id = None;
                }
            }
            save_session(&self.storage, &session)?;
        }

        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "job_id": params.job_id }),
            None,
        )?;
        Ok(Value::Null)
    }
}

pub struct JobKeepWaitingTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl JobKeepWaitingTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for JobKeepWaitingTool {
    fn name(&self) -> &str {
        "job.keep_waiting"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobIdParams>(params.clone())?;
        let mut session = load_session(&self.storage)?;
        let job = session
            .jobs
            .get_mut(&params.job_id)
            .ok_or_else(|| McpError::JobNotFound(params.job_id.clone()))?;
        job.status = JobStatus::Waiting;
        job.updated_at = Utc::now();
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "job_id": params.job_id }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct JobUpdateParams {
    job_id: String,
    new_intent: String,
}

pub struct JobUpdateTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl JobUpdateTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for JobUpdateTool {
    fn name(&self) -> &str {
        "job.update"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobUpdateParams>(params.clone())?;
        let mut session = load_session(&self.storage)?;
        let job = session
            .jobs
            .get_mut(&params.job_id)
            .ok_or_else(|| McpError::JobNotFound(params.job_id.clone()))?;
        job.intent = params.new_intent.clone();
        job.updated_at = Utc::now();
        if matches!(job.status, JobStatus::Waiting | JobStatus::Paused | JobStatus::Failed) {
            job.status = JobStatus::Pending;
        }
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "job_id": params.job_id, "new_intent": params.new_intent }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct JobRespondParams {
    request_id: String,
    answer: String,
}

pub struct JobRespondTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
}

impl JobRespondTool {
    pub fn new(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        awaiters: Arc<Mutex<RequestAwaiterRegistry>>,
    ) -> Self {
        Self {
            storage,
            push,
            awaiters,
        }
    }
}

#[async_trait]
impl Tool for JobRespondTool {
    fn name(&self) -> &str {
        "job.respond"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<JobRespondParams>(params)?;
        let mut session = load_session(&self.storage)?;
        let answer = params.answer.clone();
        let request = session.pending_requests.get_mut(&params.request_id).ok_or_else(|| {
            McpError::ValidationFailed {
                field: "request_id".to_string(),
                reason: "not found".to_string(),
            }
        })?;
        request.answer = Some(answer.clone());
        request.answered = true;
        request.answered_at = Some(Utc::now());
        let answer_to_signal = request.answer.clone().unwrap_or_default();
        save_session(&self.storage, &session)?;
        self.awaiters
            .lock()
            .await
            .signal(&params.request_id, answer_to_signal);
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "request_id": params.request_id }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::dispatcher::Tool;
    use crate::mcp::queues::RequestAwaiterRegistry;
    use crate::mcp::tools::manager::testsupport::{
        sample_pending_failover, sample_pending_request, sample_result, setup, ts, worker,
    };
    use crate::types::{ActionLogEntry, Job, JobStatus, WorkerStatus};
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::sync::Mutex;

    fn job_with_status(job_id: &str, status: JobStatus) -> Job {
        Job {
            id: job_id.to_string(),
            intent: "Implement tool".to_string(),
            status,
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
        }
    }

    #[tokio::test]
    async fn job_create_sets_pending_without_deps() {
        let (_temp, storage, push, caller) = setup();
        let tool = JobCreateTool::new(Arc::clone(&storage), Arc::clone(&push));

        let job_id = tool
            .call(json!({"intent":"Build feature"}), &caller)
            .await
            .unwrap();

        let session = storage.load_session().unwrap().unwrap();
        let job = &session.jobs[job_id.as_str().unwrap()];
        assert_eq!(job.status, JobStatus::Pending);
    }

    #[tokio::test]
    async fn job_create_waits_for_incomplete_deps() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session
            .jobs
            .insert("job_001".to_string(), job_with_status("job_001", JobStatus::Pending));
        storage.save_session(&session).unwrap();

        let tool = JobCreateTool::new(Arc::clone(&storage), Arc::clone(&push));
        let job_id = tool
            .call(json!({"intent":"Follow-up","depends_on":["job_001"]}), &caller)
            .await
            .unwrap();

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs[job_id.as_str().unwrap()].status, JobStatus::Waiting);
    }

    #[tokio::test]
    async fn job_create_is_pending_when_all_deps_completed() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session.jobs.insert(
            "job_001".to_string(),
            job_with_status("job_001", JobStatus::Completed),
        );
        storage.save_session(&session).unwrap();

        let tool = JobCreateTool::new(Arc::clone(&storage), Arc::clone(&push));
        let job_id = tool
            .call(json!({"intent":"Follow-up","depends_on":["job_001"]}), &caller)
            .await
            .unwrap();

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs[job_id.as_str().unwrap()].status, JobStatus::Pending);
    }

    #[tokio::test]
    async fn job_create_invalid_dep_returns_not_found() {
        let (_temp, storage, push, caller) = setup();
        let tool = JobCreateTool::new(storage, push);
        let error = tool
            .call(json!({"intent":"Follow-up","depends_on":["job_999"]}), &caller)
            .await
            .unwrap_err();
        assert!(matches!(error, McpError::JobNotFound(id) if id == "job_999"));
    }

    #[tokio::test]
    async fn job_create_with_worker_auto_assigns() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session.workers.insert("w1".to_string(), worker("w1", WorkerStatus::Idle));
        storage.save_session(&session).unwrap();

        let tool = JobCreateTool::new(Arc::clone(&storage), Arc::clone(&push));
        let job_id = tool
            .call(json!({"intent":"Build feature","worker_id":"w1"}), &caller)
            .await
            .unwrap();

        let session = storage.load_session().unwrap().unwrap();
        let job = &session.jobs[job_id.as_str().unwrap()];
        assert_eq!(job.status, JobStatus::Running);
        assert_eq!(job.worker_id.as_deref(), Some("w1"));
        assert_eq!(session.workers["w1"].status, WorkerStatus::Running);
    }

    #[tokio::test]
    async fn job_cancel_pending_sets_cancelled() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session
            .jobs
            .insert("job_001".to_string(), job_with_status("job_001", JobStatus::Pending));
        storage.save_session(&session).unwrap();

        let tool = JobCancelTool::new(Arc::clone(&storage), Arc::clone(&push));
        tool.call(json!({"job_id":"job_001"}), &caller).await.unwrap();

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].status, JobStatus::Cancelled);
    }

    #[tokio::test]
    async fn job_cancel_running_sets_cancelling_and_pushes_notification() {
        let (_temp, storage, push, caller) = setup();
        let (client, server) = UnixStream::pair().unwrap();
        let (_, write_half) = tokio::io::split(server);
        push.write().await.register("w1", write_half);

        let mut session = storage.load_session().unwrap().unwrap();
        session.workers.insert("w1".to_string(), worker("w1", WorkerStatus::Running));
        session.jobs.insert(
            "job_001".to_string(),
            Job {
                worker_id: Some("w1".to_string()),
                ..job_with_status("job_001", JobStatus::Running)
            },
        );
        storage.save_session(&session).unwrap();

        let tool = JobCancelTool::new(Arc::clone(&storage), Arc::clone(&push));
        tool.call(json!({"job_id":"job_001"}), &caller).await.unwrap();

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].status, JobStatus::Cancelling);

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["method"], "kingdom.cancel_job");
        assert_eq!(value["params"]["job_id"], "job_001");
    }

    #[tokio::test]
    async fn job_cancel_logs_cascade_warning() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session
            .jobs
            .insert("job_001".to_string(), job_with_status("job_001", JobStatus::Pending));
        session.jobs.insert(
            "job_002".to_string(),
            Job {
                depends_on: vec!["job_001".to_string()],
                ..job_with_status("job_002", JobStatus::Waiting)
            },
        );
        storage.save_session(&session).unwrap();

        let tool = JobCancelTool::new(Arc::clone(&storage), Arc::clone(&push));
        tool.call(json!({"job_id":"job_001"}), &caller).await.unwrap();

        let entries = storage.read_action_log(None).unwrap();
        assert!(entries.iter().any(|entry| {
            entry.action == "job.cancel"
                && entry.params["warning"] == "cancel_cascade"
                && entry.params["affected_jobs"] == json!(["job_002"])
        }));
    }

    #[tokio::test]
    async fn job_respond_sets_answer_and_omits_answer_from_log() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session
            .pending_requests
            .insert("req_001".to_string(), sample_pending_request());
        storage.save_session(&session).unwrap();

        let tool = JobRespondTool::new(
            Arc::clone(&storage),
            Arc::clone(&push),
            Arc::new(Mutex::new(RequestAwaiterRegistry::new())),
        );
        tool.call(json!({"request_id":"req_001","answer":"Ship it"}), &caller)
            .await
            .unwrap();

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(
            session.pending_requests["req_001"].answer.as_deref(),
            Some("Ship it")
        );
        let entries = storage.read_action_log(None).unwrap();
        let entry = entries.iter().find(|entry| entry.action == "job.respond").unwrap();
        assert_eq!(entry.params, json!({"request_id":"req_001"}));
        let entry_json = serde_json::to_string(entry).unwrap();
        assert!(!entry_json.contains("Ship it"));
    }

    #[tokio::test]
    async fn job_update_waiting_resets_to_pending() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session
            .jobs
            .insert("job_001".to_string(), job_with_status("job_001", JobStatus::Waiting));
        storage.save_session(&session).unwrap();

        let tool = JobUpdateTool::new(Arc::clone(&storage), Arc::clone(&push));
        tool.call(json!({"job_id":"job_001","new_intent":"Rewrite impl"}), &caller)
            .await
            .unwrap();

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].status, JobStatus::Pending);
        assert_eq!(session.jobs["job_001"].intent, "Rewrite impl");
    }

    #[tokio::test]
    async fn job_status_and_result_return_expected_payloads() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        let mut job = job_with_status("job_001", JobStatus::Completed);
        job.branch = Some("kingdom/job_001".to_string());
        job.result = Some(sample_result());
        session.jobs.insert("job_001".to_string(), job.clone());
        storage.save_session(&session).unwrap();
        storage.save_result("job_001", &sample_result()).unwrap();

        let status_tool = JobStatusTool::new(Arc::clone(&storage), Arc::clone(&push));
        let result_tool = JobResultTool::new(Arc::clone(&storage), Arc::clone(&push));

        let status: JobStatusResponse = serde_json::from_value(
            status_tool
                .call(json!({"job_id":"job_001"}), &caller)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status.status, JobStatus::Completed);

        let result: JobResultResponse = serde_json::from_value(
            result_tool
                .call(json!({"job_id":"job_001"}), &caller)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result.summary, "Implemented feature");
        assert_eq!(result.branch.as_deref(), Some("kingdom/job_001"));
    }

    #[tokio::test]
    async fn all_mutating_job_tools_write_action_log_and_workspace_log_reads_entries() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session.workers.insert("w1".to_string(), worker("w1", WorkerStatus::Idle));
        session
            .jobs
            .insert("job_001".to_string(), job_with_status("job_001", JobStatus::Waiting));
        session
            .pending_requests
            .insert("req_001".to_string(), sample_pending_request());
        session
            .pending_failovers
            .insert("w1".to_string(), sample_pending_failover());
        storage.save_session(&session).unwrap();

        JobCreateTool::new(Arc::clone(&storage), Arc::clone(&push))
            .call(json!({"intent":"New job"}), &caller)
            .await
            .unwrap();
        JobKeepWaitingTool::new(Arc::clone(&storage), Arc::clone(&push))
            .call(json!({"job_id":"job_001"}), &caller)
            .await
            .unwrap();
        JobUpdateTool::new(Arc::clone(&storage), Arc::clone(&push))
            .call(json!({"job_id":"job_001","new_intent":"Updated"}), &caller)
            .await
            .unwrap();
        JobRespondTool::new(
            Arc::clone(&storage),
            Arc::clone(&push),
            Arc::new(Mutex::new(RequestAwaiterRegistry::new())),
        )
            .call(json!({"request_id":"req_001","answer":"yes"}), &caller)
            .await
            .unwrap();

        let entries = storage.read_action_log(None).unwrap();
        let actions = entries.iter().map(|entry| entry.action.as_str()).collect::<Vec<_>>();
        assert!(actions.contains(&"job.create"));
        assert!(actions.contains(&"job.keep_waiting"));
        assert!(actions.contains(&"job.update"));
        assert!(actions.contains(&"job.respond"));

        let log_tool = crate::mcp::tools::manager::workspace::WorkspaceLogTool::new(storage, push);
        let log_value = log_tool.call(json!({"limit":50}), &caller).await.unwrap();
        let logged: Vec<ActionLogEntry> = serde_json::from_value(log_value).unwrap();
        assert!(!logged.is_empty());
    }
}
