use super::{caller_job_id, create_job, worker_id};
use crate::mcp::dispatcher::{Dispatcher, Tool};
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::queues::NotificationQueue;
use crate::mcp::server::ConnectedClient;
use crate::mcp::tools::manager::{append_action_log, load_session, parse_params, save_session};
use crate::storage::Storage;
use crate::types::{ManagerNotification, Permission, WorkerRole};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub fn register(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    notifications: Arc<Mutex<NotificationQueue>>,
) {
    dispatcher.register(Box::new(SubtaskCreateTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
        Arc::clone(&notifications),
    )));
    dispatcher.register(Box::new(WorkerNotifyTool::new(storage, push)));
}

#[derive(Deserialize)]
struct SubtaskCreateParams {
    intent: String,
    depends_on: Option<Vec<String>>,
}

pub struct SubtaskCreateTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    notifications: Arc<Mutex<NotificationQueue>>,
}

impl SubtaskCreateTool {
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
impl Tool for SubtaskCreateTool {
    fn name(&self) -> &str {
        "subtask.create"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<SubtaskCreateParams>(params)?;
        let mut session = load_session(&self.storage)?;
        let wid = worker_id(caller)?;
        let worker = session
            .workers
            .get(&wid)
            .ok_or_else(|| McpError::WorkerNotFound(wid.clone()))?;
        if !worker.permissions.contains(&Permission::SubtaskCreate) {
            return Err(McpError::Unauthorized {
                tool: self.name().to_string(),
                role: "worker".to_string(),
            });
        }
        let parent_job_id = caller_job_id(&session, caller)?;
        let mut depends_on = params.depends_on.unwrap_or_default();
        if !depends_on.contains(&parent_job_id) {
            depends_on.push(parent_job_id.clone());
        }
        let subtask_job_id = create_job(&mut session, params.intent.clone(), depends_on)?;
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({ "intent": params.intent, "depends_on": session.jobs[&subtask_job_id].depends_on }),
            Some(json!({ "job_id": subtask_job_id })),
        )?;
        self.notifications
            .lock()
            .await
            .push(ManagerNotification::SubtaskCreated {
                parent_job_id,
                subtask_job_id: subtask_job_id.clone(),
                intent: params.intent,
            });
        Ok(Value::String(subtask_job_id))
    }
}

#[derive(Deserialize)]
struct WorkerNotifyParams {
    target_worker_id: String,
    message: String,
}

pub struct WorkerNotifyTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkerNotifyTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkerNotifyTool {
    fn name(&self) -> &str {
        "worker.notify"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<WorkerNotifyParams>(params)?;
        let session = load_session(&self.storage)?;
        let wid = worker_id(caller)?;
        let worker = session
            .workers
            .get(&wid)
            .ok_or_else(|| McpError::WorkerNotFound(wid.clone()))?;
        if !worker.permissions.contains(&Permission::WorkerNotify) {
            return Err(McpError::Unauthorized {
                tool: self.name().to_string(),
                role: "worker".to_string(),
            });
        }
        if !session.workers.contains_key(&params.target_worker_id) {
            return Err(McpError::WorkerNotFound(params.target_worker_id));
        }
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({
                "target_worker_id": params.target_worker_id,
                "message_length": params.message.len()
            }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::dispatcher::Tool;
    use crate::mcp::tools::worker::testsupport::{grant_permission, setup_worker};
    use crate::types::Permission;
    use serde_json::json;

    #[tokio::test]
    async fn subtask_create_requires_permission() {
        let (_temp, storage, push, notifications, _health, _awaiters, caller) = setup_worker();
        let tool = SubtaskCreateTool::new(storage, push, notifications);
        let err = tool
            .call(json!({"intent":"child task"}), &caller)
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Unauthorized { tool, .. } if tool == "subtask.create"));
    }

    #[tokio::test]
    async fn subtask_create_includes_parent_dep() {
        let (_temp, storage, push, notifications, _health, _awaiters, caller) = setup_worker();
        grant_permission(&storage, "w1", Permission::SubtaskCreate);
        let tool = SubtaskCreateTool::new(Arc::clone(&storage), push, Arc::clone(&notifications));
        let value = tool
            .call(json!({"intent":"child task"}), &caller)
            .await
            .unwrap();
        let session = storage.load_session().unwrap().unwrap();
        assert!(session.jobs[value.as_str().unwrap()]
            .depends_on
            .contains(&"job_001".to_string()));
    }
}
