use super::{append_action_log, load_session, parse_params, save_session};
use crate::failover::machine::FailoverCommand;
use crate::mcp::dispatcher::{Dispatcher, Tool};
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::server::ConnectedClient;
use crate::storage::Storage;
use crate::types::{PendingFailoverStatus, WorkerRole};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

pub fn register(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
) {
    register_with_machine(dispatcher, storage, push, None);
}

pub fn register_with_machine(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    command_tx: Option<mpsc::Sender<FailoverCommand>>,
) {
    dispatcher.register(Box::new(FailoverConfirmTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
        command_tx.clone(),
    )));
    dispatcher.register(Box::new(FailoverCancelTool::new(storage, push, command_tx)));
}

#[derive(Deserialize)]
struct FailoverConfirmParams {
    worker_id: String,
    new_provider: String,
}

pub struct FailoverConfirmTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    command_tx: Option<mpsc::Sender<FailoverCommand>>,
}

impl FailoverConfirmTool {
    pub fn new(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        command_tx: Option<mpsc::Sender<FailoverCommand>>,
    ) -> Self {
        Self {
            storage,
            push,
            command_tx,
        }
    }
}

#[async_trait]
impl Tool for FailoverConfirmTool {
    fn name(&self) -> &str {
        "failover.confirm"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<FailoverConfirmParams>(params.clone())?;
        let mut session = load_session(&self.storage)?;
        let worker_id = params.worker_id.clone();
        let new_provider = params.new_provider.clone();
        let failover = session
            .pending_failovers
            .get_mut(&worker_id)
            .ok_or_else(|| McpError::WorkerNotFound(worker_id.clone()))?;
        failover.status = PendingFailoverStatus::Confirmed {
            new_provider: params.new_provider.clone(),
        };
        save_session(&self.storage, &session)?;
        if let Some(command_tx) = &self.command_tx {
            command_tx
                .send(FailoverCommand::Confirm {
                    worker_id: worker_id.clone(),
                    new_provider: new_provider.clone(),
                })
                .await
                .map_err(|_| McpError::Internal("failover machine unavailable".to_string()))?;
        }
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            serde_json::json!({ "worker_id": worker_id, "new_provider": new_provider }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct FailoverCancelParams {
    worker_id: String,
}

pub struct FailoverCancelTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    command_tx: Option<mpsc::Sender<FailoverCommand>>,
}

impl FailoverCancelTool {
    pub fn new(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        command_tx: Option<mpsc::Sender<FailoverCommand>>,
    ) -> Self {
        Self {
            storage,
            push,
            command_tx,
        }
    }
}

#[async_trait]
impl Tool for FailoverCancelTool {
    fn name(&self) -> &str {
        "failover.cancel"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<FailoverCancelParams>(params.clone())?;
        let mut session = load_session(&self.storage)?;
        let worker_id = params.worker_id.clone();
        let failover = session
            .pending_failovers
            .get_mut(&worker_id)
            .ok_or_else(|| McpError::WorkerNotFound(worker_id.clone()))?;
        failover.status = PendingFailoverStatus::Cancelled;
        save_session(&self.storage, &session)?;
        if let Some(command_tx) = &self.command_tx {
            command_tx
                .send(FailoverCommand::Cancel {
                    worker_id: worker_id.clone(),
                })
                .await
                .map_err(|_| McpError::Internal("failover machine unavailable".to_string()))?;
        }
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            serde_json::json!({ "worker_id": worker_id }),
            None,
        )?;
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::dispatcher::Tool;
    use crate::mcp::tools::manager::testsupport::{sample_pending_failover, setup};
    use crate::types::PendingFailoverStatus;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn failover_confirm_and_cancel_update_status() {
        let (_temp, storage, push, caller) = setup();
        let (tx, mut rx) = mpsc::channel(4);
        let mut session = storage.load_session().unwrap().unwrap();
        session
            .pending_failovers
            .insert("w1".to_string(), sample_pending_failover());
        storage.save_session(&session).unwrap();

        let confirm =
            FailoverConfirmTool::new(Arc::clone(&storage), Arc::clone(&push), Some(tx.clone()));
        confirm
            .call(json!({"worker_id":"w1","new_provider":"gemini"}), &caller)
            .await
            .unwrap();

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(
            session.pending_failovers["w1"].status,
            PendingFailoverStatus::Confirmed {
                new_provider: "gemini".to_string()
            }
        );
        assert_eq!(
            rx.recv().await,
            Some(FailoverCommand::Confirm {
                worker_id: "w1".to_string(),
                new_provider: "gemini".to_string(),
            })
        );

        let cancel = FailoverCancelTool::new(Arc::clone(&storage), Arc::clone(&push), Some(tx));
        cancel
            .call(json!({"worker_id":"w1"}), &caller)
            .await
            .unwrap();
        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(
            session.pending_failovers["w1"].status,
            PendingFailoverStatus::Cancelled
        );
        assert_eq!(
            rx.recv().await,
            Some(FailoverCommand::Cancel {
                worker_id: "w1".to_string(),
            })
        );
    }
}
