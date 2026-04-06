use super::{context_urgency, worker_id};
use crate::mcp::dispatcher::{Dispatcher, Tool};
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::queues::HealthEventQueue;
use crate::mcp::server::ConnectedClient;
use crate::mcp::tools::manager::{append_action_log, load_session, parse_params, save_session};
use crate::storage::Storage;
use crate::types::{HealthEvent, WorkerRole};
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
    health_events: Arc<Mutex<HealthEventQueue>>,
) {
    dispatcher.register(Box::new(ContextPingTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
        Arc::clone(&health_events),
    )));
    dispatcher.register(Box::new(ContextCheckpointDeferTool::new(storage, push)));
}

#[derive(Deserialize)]
struct ContextPingParams {
    usage_pct: f32,
    token_count: u64,
}

pub struct ContextPingTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
    health_events: Arc<Mutex<HealthEventQueue>>,
}

impl ContextPingTool {
    pub fn new(
        storage: Arc<Storage>,
        push: Arc<RwLock<PushRegistry>>,
        health_events: Arc<Mutex<HealthEventQueue>>,
    ) -> Self {
        Self {
            storage,
            push,
            health_events,
        }
    }
}

#[async_trait]
impl Tool for ContextPingTool {
    fn name(&self) -> &str {
        "context.ping"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<ContextPingParams>(params)?;
        let mut session = load_session(&self.storage)?;
        let worker_id = worker_id(caller)?;
        let job_id = {
            let worker = session
                .workers
                .get_mut(&worker_id)
                .ok_or_else(|| McpError::WorkerNotFound(worker_id.clone()))?;
            worker.context_usage_pct = Some(params.usage_pct);
            worker.token_count = Some(params.token_count);
            worker.last_heartbeat = Some(Utc::now());
            worker.job_id.clone()
        };
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({
                "worker_id": worker_id,
                "job_id": job_id,
                "token_count": params.token_count,
                "usage_pct": params.usage_pct,
            }),
            None,
        )?;
        if let Some(urgency) = context_urgency(params.usage_pct) {
            self.health_events
                .lock()
                .await
                .push(HealthEvent::ContextThreshold {
                    worker_id,
                    pct: params.usage_pct,
                    urgency,
                });
        }
        Ok(Value::Null)
    }
}

#[derive(Deserialize)]
struct ContextCheckpointDeferParams {
    job_id: String,
    reason: String,
    eta_seconds: u32,
}

pub struct ContextCheckpointDeferTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl ContextCheckpointDeferTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for ContextCheckpointDeferTool {
    fn name(&self) -> &str {
        "context.checkpoint_defer"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<ContextCheckpointDeferParams>(params)?;
        let session = load_session(&self.storage)?;
        let worker = &session.workers[&worker_id(caller)?];
        if let Some(usage_pct) = worker.context_usage_pct {
            if matches!(
                context_urgency(usage_pct),
                Some(crate::types::CheckpointUrgency::Critical)
            ) {
                return Err(McpError::ValidationFailed {
                    field: "urgency".to_string(),
                    reason: "cannot defer checkpoint at critical context usage".to_string(),
                });
            }
        }
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({
                "job_id": params.job_id,
                "reason": params.reason,
                "eta_seconds": params.eta_seconds
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
    use crate::mcp::tools::worker::testsupport::setup_worker;
    use crate::types::{CheckpointUrgency, HealthEvent};
    use serde_json::json;

    #[tokio::test]
    async fn context_ping_high_usage_queues_event() {
        let (_temp, storage, push, _notifications, health, _awaiters, caller) = setup_worker();
        let tool = ContextPingTool::new(Arc::clone(&storage), push, Arc::clone(&health));
        tool.call(json!({"usage_pct":0.72,"token_count":1000}), &caller)
            .await
            .unwrap();
        let events = health.lock().await.drain();
        assert!(matches!(
            &events[0],
            HealthEvent::ContextThreshold {
                urgency: CheckpointUrgency::High,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn context_ping_low_usage_has_no_event() {
        let (_temp, storage, push, _notifications, health, _awaiters, caller) = setup_worker();
        let tool = ContextPingTool::new(storage, push, Arc::clone(&health));
        tool.call(json!({"usage_pct":0.30,"token_count":1000}), &caller)
            .await
            .unwrap();
        assert!(health.lock().await.drain().is_empty());
    }

    #[tokio::test]
    async fn context_ping_writes_action_log() {
        let (_temp, storage, push, _notifications, health, _awaiters, caller) = setup_worker();
        let tool = ContextPingTool::new(Arc::clone(&storage), push, Arc::clone(&health));
        tool.call(json!({"usage_pct":0.30,"token_count":1234}), &caller)
            .await
            .unwrap();

        let entries = storage.read_action_log(None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].actor, "w1");
        assert_eq!(entries[0].action, "context.ping");
        assert_eq!(entries[0].params["worker_id"], json!("w1"));
        assert_eq!(entries[0].params["job_id"], json!("job_001"));
        assert_eq!(entries[0].params["token_count"], json!(1234));
    }

    #[tokio::test]
    async fn checkpoint_defer_critical_fails_and_high_succeeds() {
        let (_temp, storage, push, _notifications, _health, _awaiters, caller) = setup_worker();
        let mut session = storage.load_session().unwrap().unwrap();
        session.workers.get_mut("w1").unwrap().context_usage_pct = Some(0.9);
        storage.save_session(&session).unwrap();
        let tool = ContextCheckpointDeferTool::new(Arc::clone(&storage), Arc::clone(&push));
        let err = tool
            .call(
                json!({"job_id":"job_001","reason":"later","eta_seconds":30}),
                &caller,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ValidationFailed { field, .. } if field == "urgency"));

        let mut session = storage.load_session().unwrap().unwrap();
        session.workers.get_mut("w1").unwrap().context_usage_pct = Some(0.72);
        storage.save_session(&session).unwrap();
        tool.call(
            json!({"job_id":"job_001","reason":"later","eta_seconds":30}),
            &caller,
        )
        .await
        .unwrap();
    }
}
