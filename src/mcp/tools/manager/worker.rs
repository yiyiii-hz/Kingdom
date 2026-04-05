use super::{
    append_action_log, assign_job, load_session, parse_params, permission_from_str, save_session,
};
use crate::mcp::dispatcher::{Dispatcher, Tool};
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::server::ConnectedClient;
use crate::storage::Storage;
use crate::types::{Worker, WorkerRole, WorkerStatus};
use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

pub fn register(dispatcher: &mut Dispatcher, storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) {
    dispatcher.register(Box::new(WorkerCreateTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
        None,
    )));
    dispatcher.register(Box::new(WorkerAssignTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(WorkerReleaseTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(WorkerGrantTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(WorkerRevokeTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(WorkerSwapTool::new(storage, push)));
}

pub fn register_with_launcher(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    launcher: Arc<crate::process::launcher::ProcessLauncher>,
) {
    dispatcher.register(Box::new(WorkerCreateTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
        Some(launcher),
    )));
    dispatcher.register(Box::new(WorkerAssignTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(WorkerReleaseTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(WorkerGrantTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(WorkerRevokeTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(WorkerSwapTool::new(storage, push)));
}

pub struct WorkerCreateTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    launcher: Option<Arc<crate::process::launcher::ProcessLauncher>>,
}

impl WorkerCreateTool {
    pub fn new(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        launcher: Option<Arc<crate::process::launcher::ProcessLauncher>>,
    ) -> Self {
        Self {
            storage,
            push,
            launcher,
        }
    }
}

#[async_trait]
impl Tool for WorkerCreateTool {
    fn name(&self) -> &str {
        "worker.create"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        #[derive(serde::Deserialize)]
        struct Params {
            provider: String,
            role: Option<String>,
        }
        let p: Params = parse_params(params.clone())?;
        let role = match p.role.as_deref() {
            Some("manager") => WorkerRole::Manager,
            _ => WorkerRole::Worker,
        };

        let mut session = load_session(&self.storage)?;
        let config =
            crate::config::KingdomConfig::load_or_default(&self.storage.root.join("config.toml"));
        if crate::process::discovery::ProviderDiscovery::check(&p.provider, &config).is_none() {
            return Err(McpError::ValidationFailed {
                field: "provider".to_string(),
                reason: format!("provider '{}' not found", p.provider),
            });
        }

        let worker_id = format!("w{}", session.worker_seq + 1);
        let worker_index = session.workers.len();

        let (pid, pane_id) = if let Some(launcher) = &self.launcher {
            let result = launcher
                .launch(
                    &p.provider,
                    role.clone(),
                    &worker_id,
                    worker_index,
                    &self.storage.root,
                )
                .await
                .map_err(|e| McpError::Internal(e.to_string()))?;
            (Some(result.pid), result.pane_id)
        } else {
            (None, String::new())
        };

        let worker = Worker {
            id: worker_id.clone(),
            provider: p.provider,
            role,
            status: WorkerStatus::Starting,
            job_id: None,
            pid,
            pane_id,
            mcp_connected: false,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: None,
            last_progress: None,
            permissions: vec![],
            started_at: Utc::now(),
        };

        session.workers.insert(worker_id.clone(), worker);
        session.worker_seq += 1;
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            params,
            None,
        )?;
        Ok(json!({ "worker_id": worker_id }))
    }
}

#[derive(Deserialize)]
struct WorkerAssignParams {
    worker_id: String,
    job_id: String,
}

pub struct WorkerAssignTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkerAssignTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkerAssignTool {
    fn name(&self) -> &str {
        "worker.assign"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<WorkerAssignParams>(params.clone())?;
        let mut session = load_session(&self.storage)?;
        assign_job(&mut session, &params.worker_id, &params.job_id)?;
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "worker_id": params.worker_id, "job_id": params.job_id }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct WorkerReleaseParams {
    worker_id: String,
}

pub struct WorkerReleaseTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkerReleaseTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkerReleaseTool {
    fn name(&self) -> &str {
        "worker.release"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<WorkerReleaseParams>(params.clone())?;
        let mut session = load_session(&self.storage)?;
        let worker = super::worker_mut(&mut session, &params.worker_id)?;
        if worker.status != WorkerStatus::Idle {
            return Err(McpError::InvalidState {
                message: "WORKER_NOT_IDLE".to_string(),
            });
        }
        worker.status = WorkerStatus::Terminated;
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "worker_id": params.worker_id }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct WorkerPermissionParams {
    worker_id: String,
    permission: String,
}

pub struct WorkerGrantTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkerGrantTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkerGrantTool {
    fn name(&self) -> &str {
        "worker.grant"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<WorkerPermissionParams>(params.clone())?;
        let permission = permission_from_str(&params.permission)?;
        let mut session = load_session(&self.storage)?;
        let worker = super::worker_mut(&mut session, &params.worker_id)?;
        if !worker.permissions.contains(&permission) {
            worker.permissions.push(permission);
        }
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "worker_id": params.worker_id, "permission": params.permission }),
            None,
        )?;
        Ok(Value::Null)
    }
}

pub struct WorkerRevokeTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkerRevokeTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkerRevokeTool {
    fn name(&self) -> &str {
        "worker.revoke"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<WorkerPermissionParams>(params.clone())?;
        let permission = permission_from_str(&params.permission)?;
        let mut session = load_session(&self.storage)?;
        let worker = super::worker_mut(&mut session, &params.worker_id)?;
        worker.permissions.retain(|value| value != &permission);
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "worker_id": params.worker_id, "permission": params.permission }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct WorkerSwapParams {
    worker_id: String,
    new_provider: String,
}

pub struct WorkerSwapTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkerSwapTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkerSwapTool {
    fn name(&self) -> &str {
        "worker.swap"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<WorkerSwapParams>(params.clone())?;
        let session = load_session(&self.storage)?;
        if !session.workers.contains_key(&params.worker_id) {
            return Err(McpError::WorkerNotFound(params.worker_id));
        }
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "worker_id": params.worker_id, "new_provider": params.new_provider }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::dispatcher::Tool;
    use crate::mcp::tools::manager::testsupport::{setup, ts, worker};
    use crate::types::{Job, JobStatus, Permission, WorkerStatus};
    use serde_json::json;

    fn pending_job(job_id: &str) -> Job {
        Job {
            id: job_id.to_string(),
            intent: "Implement M3".to_string(),
            status: JobStatus::Pending,
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
    async fn worker_assign_updates_worker_and_job() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session.workers.insert("w1".to_string(), worker("w1", WorkerStatus::Idle));
        session.jobs.insert("job_001".to_string(), pending_job("job_001"));
        storage.save_session(&session).unwrap();

        let tool = WorkerAssignTool::new(Arc::clone(&storage), Arc::clone(&push));
        tool.call(json!({"worker_id":"w1","job_id":"job_001"}), &caller)
            .await
            .unwrap();

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].status, JobStatus::Running);
        assert_eq!(session.jobs["job_001"].worker_id.as_deref(), Some("w1"));
        assert_eq!(session.workers["w1"].status, WorkerStatus::Running);
    }

    #[tokio::test]
    async fn worker_assign_running_worker_returns_busy() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session
            .workers
            .insert("w1".to_string(), worker("w1", WorkerStatus::Running));
        session.jobs.insert("job_001".to_string(), pending_job("job_001"));
        storage.save_session(&session).unwrap();

        let tool = WorkerAssignTool::new(storage, push);
        let error = tool
            .call(json!({"worker_id":"w1","job_id":"job_001"}), &caller)
            .await
            .unwrap_err();
        assert!(matches!(error, McpError::InvalidState { message } if message == "WORKER_BUSY"));
    }

    #[tokio::test]
    async fn worker_assign_non_pending_job_returns_error() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session.workers.insert("w1".to_string(), worker("w1", WorkerStatus::Idle));
        let mut job = pending_job("job_001");
        job.status = JobStatus::Waiting;
        session.jobs.insert(job.id.clone(), job);
        storage.save_session(&session).unwrap();

        let tool = WorkerAssignTool::new(storage, push);
        let error = tool
            .call(json!({"worker_id":"w1","job_id":"job_001"}), &caller)
            .await
            .unwrap_err();
        assert!(matches!(error, McpError::InvalidState { message } if message == "JOB_NOT_PENDING"));
    }

    #[tokio::test]
    async fn worker_release_running_worker_returns_invalid_state() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session
            .workers
            .insert("w1".to_string(), worker("w1", WorkerStatus::Running));
        storage.save_session(&session).unwrap();

        let tool = WorkerReleaseTool::new(storage, push);
        let error = tool
            .call(json!({"worker_id":"w1"}), &caller)
            .await
            .unwrap_err();
        assert!(matches!(error, McpError::InvalidState { .. }));
    }

    #[tokio::test]
    async fn worker_release_idle_worker_terminates() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session.workers.insert("w1".to_string(), worker("w1", WorkerStatus::Idle));
        storage.save_session(&session).unwrap();

        let tool = WorkerReleaseTool::new(Arc::clone(&storage), Arc::clone(&push));
        tool.call(json!({"worker_id":"w1"}), &caller).await.unwrap();

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.workers["w1"].status, WorkerStatus::Terminated);
    }

    #[tokio::test]
    async fn worker_grant_is_idempotent_and_revoke_removes_permission() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session.workers.insert("w1".to_string(), worker("w1", WorkerStatus::Idle));
        storage.save_session(&session).unwrap();

        let grant = WorkerGrantTool::new(Arc::clone(&storage), Arc::clone(&push));
        grant
            .call(json!({"worker_id":"w1","permission":"workspace_read"}), &caller)
            .await
            .unwrap();
        grant
            .call(json!({"worker_id":"w1","permission":"workspace_read"}), &caller)
            .await
            .unwrap();

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.workers["w1"].permissions, vec![Permission::WorkspaceRead]);

        let revoke = WorkerRevokeTool::new(Arc::clone(&storage), Arc::clone(&push));
        revoke
            .call(json!({"worker_id":"w1","permission":"workspace_read"}), &caller)
            .await
            .unwrap();
        let session = storage.load_session().unwrap().unwrap();
        assert!(session.workers["w1"].permissions.is_empty());
    }
}
