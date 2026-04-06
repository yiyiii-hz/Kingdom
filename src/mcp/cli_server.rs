use crate::cli::swap::queue_manual_swap;
use crate::failover::machine::FailoverCommand;
use crate::mcp::push::PushRegistry;
use crate::storage::Storage;
use serde_json::{json, Value};
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, RwLock};

#[derive(Debug)]
pub enum CliServerError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl Display for CliServerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for CliServerError {}

impl From<std::io::Error> for CliServerError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for CliServerError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub struct CliServer {
    workspace_hash: String,
    workspace: PathBuf,
    storage: Arc<Storage>,
    failover_tx: mpsc::Sender<FailoverCommand>,
    push: Arc<RwLock<PushRegistry>>,
}

impl CliServer {
    pub fn new(
        workspace_hash: &str,
        workspace: PathBuf,
        storage: Arc<Storage>,
        failover_tx: mpsc::Sender<FailoverCommand>,
        push: Arc<RwLock<PushRegistry>>,
    ) -> Self {
        Self {
            workspace_hash: workspace_hash.to_string(),
            workspace,
            storage,
            failover_tx,
            push,
        }
    }

    pub async fn start(&self) -> Result<(), CliServerError> {
        tokio::fs::create_dir_all("/tmp/kingdom").await?;
        let path = self.socket_path();
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }

        let listener = UnixListener::bind(path)?;
        let workspace = self.workspace.clone();
        let storage = Arc::clone(&self.storage);
        let failover_tx = self.failover_tx.clone();
        let push = Arc::clone(&self.push);
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(value) => value,
                    Err(_) => break,
                };

                let workspace = workspace.clone();
                let storage = Arc::clone(&storage);
                let failover_tx = failover_tx.clone();
                let push = Arc::clone(&push);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = tokio::io::split(stream);
                    let mut reader = BufReader::new(read_half);
                    let mut line = String::new();

                    if reader
                        .read_line(&mut line)
                        .await
                        .ok()
                        .filter(|bytes| *bytes > 0)
                        .is_none()
                    {
                        return;
                    }

                    let response = match serde_json::from_str::<Value>(&line) {
                        Ok(request) => {
                            handle_command(&workspace, &storage, &failover_tx, &push, &request)
                                .await
                        }
                        Err(_) => json!({"ok": false, "error": "invalid json"}),
                    };

                    if let Ok(mut bytes) = serde_json::to_vec(&response) {
                        bytes.push(b'\n');
                        let _ = write_half.write_all(&bytes).await;
                        let _ = write_half.flush().await;
                    }
                });
            }
        });

        Ok(())
    }

    fn socket_path(&self) -> PathBuf {
        PathBuf::from(format!("/tmp/kingdom/{}-cli.sock", self.workspace_hash))
    }
}

async fn handle_command(
    workspace: &std::path::Path,
    storage: &Arc<Storage>,
    failover_tx: &mpsc::Sender<FailoverCommand>,
    push: &Arc<RwLock<PushRegistry>>,
    request: &Value,
) -> Value {
    match request.get("cmd").and_then(Value::as_str) {
        Some("ready") => json!({"ok": true, "data": {"status": "ready"}}),
        Some("swap") => {
            let worker_id = match request.get("worker_id").and_then(Value::as_str) {
                Some(worker_id) => worker_id,
                None => return json!({"ok": false, "error": "missing worker_id"}),
            };
            let provider = request
                .get("provider")
                .and_then(Value::as_str)
                .map(|provider| provider.to_string());
            match queue_manual_swap(
                storage,
                workspace,
                worker_id,
                provider,
                Some(failover_tx.clone()),
                Some(Arc::clone(push)),
            )
            .await
            {
                Ok(provider) => json!({"ok": true, "data": { "provider": provider }}),
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            }
        }
        Some("status") => json!({"ok": true, "data": {}}),
        Some("log") => json!({"ok": true, "data": {"entries": []}}),
        Some("shutdown") => json!({"ok": true, "data": {}}),
        Some(command) => json!({"ok": false, "error": format!("unknown command: {command}")}),
        None => json!({"ok": false, "error": "unknown command: "}),
    }
}

#[cfg(test)]
mod tests {
    use super::CliServer;
    use crate::mcp::push::PushRegistry;
    use crate::storage::Storage;
    use crate::types::{
        CheckpointContent, CheckpointMeta, GitStrategy, Job, JobStatus, NotificationMode, Session,
        Worker, WorkerRole, WorkerStatus,
    };
    use chrono::Utc;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::sync::{mpsc, RwLock};

    fn ts() -> chrono::DateTime<Utc> {
        Utc::now()
    }

    fn session(root: &std::path::Path) -> Session {
        Session {
            id: "sess_1".to_string(),
            workspace_path: root.display().to_string(),
            workspace_hash: "m2-cli-swap".to_string(),
            manager_id: Some("wm".to_string()),
            workers: [
                (
                    "wm".to_string(),
                    Worker {
                        id: "wm".to_string(),
                        provider: "claude".to_string(),
                        role: WorkerRole::Manager,
                        status: WorkerStatus::Idle,
                        job_id: None,
                        pid: None,
                        pane_id: "%0".to_string(),
                        mcp_connected: true,
                        context_usage_pct: None,
                        token_count: None,
                        last_heartbeat: None,
                        last_progress: None,
                        permissions: vec![],
                        started_at: ts(),
                    },
                ),
                (
                    "w1".to_string(),
                    Worker {
                        id: "w1".to_string(),
                        provider: "codex".to_string(),
                        role: WorkerRole::Worker,
                        status: WorkerStatus::Running,
                        job_id: Some("job_001".to_string()),
                        pid: None,
                        pane_id: "%1".to_string(),
                        mcp_connected: true,
                        context_usage_pct: None,
                        token_count: None,
                        last_heartbeat: None,
                        last_progress: None,
                        permissions: vec![],
                        started_at: ts(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
            jobs: [(
                "job_001".to_string(),
                Job {
                    id: "job_001".to_string(),
                    intent: "swap me".to_string(),
                    status: JobStatus::Running,
                    worker_id: Some("w1".to_string()),
                    depends_on: vec![],
                    created_at: ts(),
                    updated_at: ts(),
                    branch: None,
                    branch_start_commit: None,
                    checkpoints: vec![CheckpointMeta {
                        id: "ckpt_1".to_string(),
                        job_id: "job_001".to_string(),
                        created_at: ts(),
                        git_commit: None,
                    }],
                    result: None,
                    fail_count: 0,
                    last_fail_at: None,
                },
            )]
            .into_iter()
            .collect(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: GitStrategy::None,
            available_providers: vec![
                "claude".to_string(),
                "codex".to_string(),
                "gemini".to_string(),
            ],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: ts(),
        }
    }

    #[tokio::test]
    async fn cli_server_swap_queues_failover_and_emits_command() {
        let temp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(temp.path()).unwrap());
        storage.save_session(&session(temp.path())).unwrap();
        storage
            .save_checkpoint(&CheckpointContent {
                id: "ckpt_1".to_string(),
                job_id: "job_001".to_string(),
                created_at: ts(),
                done: "done".to_string(),
                abandoned: "".to_string(),
                in_progress: "progress".to_string(),
                remaining: "remaining".to_string(),
                pitfalls: "pitfalls".to_string(),
                git_commit: None,
            })
            .unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        let server = CliServer::new(
            "m2-cli-swap",
            temp.path().to_path_buf(),
            Arc::clone(&storage),
            tx,
            Arc::new(RwLock::new(PushRegistry::new())),
        );
        server.start().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect("/tmp/kingdom/m2-cli-swap-cli.sock")
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut bytes =
            serde_json::to_vec(&json!({"cmd":"swap","worker_id":"w1","provider":"gemini"}))
                .unwrap();
        bytes.push(b'\n');
        reader.get_mut().write_all(&bytes).await.unwrap();
        reader.get_mut().flush().await.unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["ok"], json!(true));
        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(
            session.pending_failovers["w1"].recommended_provider,
            Some("gemini".to_string())
        );
        assert_eq!(
            rx.recv().await,
            Some(crate::failover::machine::FailoverCommand::Confirm {
                worker_id: "w1".to_string(),
                new_provider: "gemini".to_string(),
            })
        );
    }
}
