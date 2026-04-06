use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

#[derive(Debug)]
pub enum PushError {
    WorkerNotRegistered(String),
    Serialize(serde_json::Error),
    Io(std::io::Error),
}

impl Display for PushError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkerNotRegistered(worker_id) => {
                write!(f, "worker not registered for push: {worker_id}")
            }
            Self::Serialize(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for PushError {}

impl From<serde_json::Error> for PushError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialize(value)
    }
}

impl From<std::io::Error> for PushError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub struct PushRegistry {
    connections: HashMap<String, Arc<Mutex<WriteHalf<UnixStream>>>>,
}

impl PushRegistry {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }

    pub fn register(&mut self, worker_id: &str, write: WriteHalf<UnixStream>) {
        self.register_shared(worker_id, Arc::new(Mutex::new(write)));
    }

    pub fn register_shared(&mut self, worker_id: &str, write: Arc<Mutex<WriteHalf<UnixStream>>>) {
        self.connections.insert(worker_id.to_string(), write);
    }

    pub fn deregister(&mut self, worker_id: &str) {
        self.connections.remove(worker_id);
    }

    pub async fn push(&self, worker_id: &str, notification: Value) -> Result<(), PushError> {
        let writer = self
            .connections
            .get(worker_id)
            .cloned()
            .ok_or_else(|| PushError::WorkerNotRegistered(worker_id.to_string()))?;

        let message = json!({
            "jsonrpc": "2.0",
            "method": notification.get("method").cloned().unwrap_or_else(|| Value::String("kingdom.event".to_string())),
            "params": notification.get("params").cloned().unwrap_or(notification),
        });

        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');

        let mut writer = writer.lock().await;
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::PushRegistry;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixStream;

    #[tokio::test]
    async fn push_writes_notification_as_jsonrpc_message() {
        let (client, server) = UnixStream::pair().unwrap();
        let (_, write_half) = tokio::io::split(server);
        let mut registry = PushRegistry::new();
        registry.register("w1", write_half);

        registry
            .push(
                "w1",
                json!({"method":"kingdom.event","params":{"type":"job_completed"}}),
            )
            .await
            .unwrap();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();

        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "kingdom.event");
        assert_eq!(parsed["params"]["type"], "job_completed");
        assert!(parsed.get("id").is_none());
    }
}
