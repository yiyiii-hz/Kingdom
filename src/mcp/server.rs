use crate::mcp::dispatcher::Dispatcher;
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::replay::RecentCalls;
use crate::storage::{Storage, StorageError};
use crate::types::{NotificationMode, WorkerRole, WorkerStatus};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{watch, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

pub struct McpServer {
    workspace_hash: String,
    storage: Arc<Storage>,
    dispatcher: Arc<Dispatcher>,
    push_registry: Arc<RwLock<PushRegistry>>,
    recent_calls: Arc<Mutex<RecentCalls>>,
    active_connections: Arc<RwLock<HashMap<String, ConnectedClient>>>,
    shutdown: watch::Sender<bool>,
    listener_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectedClient {
    pub connection_id: String,
    pub worker_id: Option<String>,
    pub role: WorkerRole,
    pub session_id: String,
}

#[derive(Debug)]
pub enum ServerError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Storage(StorageError),
}

impl Display for ServerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::Storage(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<std::io::Error> for ServerError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for ServerError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<StorageError> for ServerError {
    fn from(value: StorageError) -> Self {
        Self::Storage(value)
    }
}

impl McpServer {
    pub fn new(workspace_hash: &str, storage: Arc<Storage>) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            workspace_hash: workspace_hash.to_string(),
            storage,
            dispatcher: Arc::new(Dispatcher::new()),
            push_registry: Arc::new(RwLock::new(PushRegistry::new())),
            recent_calls: Arc::new(Mutex::new(RecentCalls::new())),
            active_connections: Arc::new(RwLock::new(HashMap::new())),
            shutdown,
            listener_task: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_dispatcher(
        workspace_hash: &str,
        storage: Arc<Storage>,
        dispatcher: Arc<Dispatcher>,
        push_registry: Arc<RwLock<PushRegistry>>,
    ) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            workspace_hash: workspace_hash.to_string(),
            storage,
            dispatcher,
            push_registry,
            recent_calls: Arc::new(Mutex::new(RecentCalls::new())),
            active_connections: Arc::new(RwLock::new(HashMap::new())),
            shutdown,
            listener_task: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn start(&self) -> Result<(), ServerError> {
        tokio::fs::create_dir_all("/tmp/kingdom").await?;
        let socket_path = self.socket_path();
        if socket_path.exists() {
            tokio::fs::remove_file(&socket_path).await?;
        }

        let listener = UnixListener::bind(&socket_path)?;
        let mut shutdown_rx = self.shutdown.subscribe();
        let storage = Arc::clone(&self.storage);
        let dispatcher = Arc::clone(&self.dispatcher);
        let push_registry = Arc::clone(&self.push_registry);
        let recent_calls = Arc::clone(&self.recent_calls);
        let active_connections = Arc::clone(&self.active_connections);

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_ok() && *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        let (stream, _) = match accepted {
                            Ok(value) => value,
                            Err(error) => {
                                tracing::error!(error = %error, "mcp accept failed");
                                break;
                            }
                        };

                        let storage = Arc::clone(&storage);
                        let dispatcher = Arc::clone(&dispatcher);
                        let push_registry = Arc::clone(&push_registry);
                        let recent_calls = Arc::clone(&recent_calls);
                        let active_connections = Arc::clone(&active_connections);

                        tokio::spawn(async move {
                            if let Err(error) = handle_connection(
                                stream,
                                storage,
                                dispatcher,
                                push_registry,
                                recent_calls,
                                active_connections,
                            )
                            .await
                            {
                                tracing::error!(error = %error, "mcp connection failed");
                            }
                        });
                    }
                }
            }
        });

        *self.listener_task.lock().await = Some(task);
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), ServerError> {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.listener_task.lock().await.take() {
            let _ = task.await;
        }

        let socket_path = self.socket_path();
        if socket_path.exists() {
            tokio::fs::remove_file(socket_path).await?;
        }
        Ok(())
    }

    fn socket_path(&self) -> PathBuf {
        PathBuf::from(format!("/tmp/kingdom/{}.sock", self.workspace_hash))
    }
}

async fn handle_connection(
    stream: UnixStream,
    storage: Arc<Storage>,
    dispatcher: Arc<Dispatcher>,
    push_registry: Arc<RwLock<PushRegistry>>,
    recent_calls: Arc<Mutex<RecentCalls>>,
    active_connections: Arc<RwLock<HashMap<String, ConnectedClient>>>,
) -> Result<(), ServerError> {
    let connection_id = Uuid::new_v4().to_string();
    tracing::info!(peer = %connection_id, "new connection");

    let (read_half, write_half) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(write_half));
    let mut reader = BufReader::new(read_half);

    let mut hello_line = String::new();
    if reader.read_line(&mut hello_line).await? == 0 {
        return Ok(());
    }

    let hello_request = match serde_json::from_str::<Value>(&hello_line) {
        Ok(value) => value,
        Err(_) => {
            write_json_line(
                &writer,
                &json!({
                    "jsonrpc":"2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": "parse error"},
                }),
            )
            .await?;
            return Ok(());
        }
    };

    if hello_request.get("method") != Some(&Value::String("kingdom.hello".to_string())) {
        write_json_line(
            &writer,
            &json!({
                "jsonrpc":"2.0",
                "id": hello_request.get("id").cloned().unwrap_or(Value::Null),
                "error": {"code": -32600, "message": "first message must be kingdom.hello"},
            }),
        )
        .await?;
        return Ok(());
    }

    let (caller, hello_response) = match perform_hello(
        &connection_id,
        &hello_request,
        &storage,
        &dispatcher,
        &push_registry,
        &active_connections,
        Arc::clone(&writer),
    )
    .await
    {
        Ok(value) => value,
        Err(error_response) => {
            tracing::warn!(
                peer = %connection_id,
                error = %json_error_message(&error_response),
                "hello rejected"
            );
            write_json_line(&writer, &error_response).await?;
            return Ok(());
        }
    };

    write_json_line(&writer, &hello_response).await?;
    tracing::info!(
        peer = %connection_id,
        worker_id = caller.worker_id.as_deref().unwrap_or(""),
        role = ?caller.role,
        "hello ok"
    );

    let dedupe_key = caller
        .worker_id
        .clone()
        .unwrap_or_else(|| caller.connection_id.clone());
    let role = caller.role.clone();
    let worker_id = caller.worker_id.clone();
    let connection_id = caller.connection_id.clone();

    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break;
        }

        let request = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(_) => {
                write_json_line(
                    &writer,
                    &json!({
                        "jsonrpc":"2.0",
                        "id": Value::Null,
                        "error": {"code": -32700, "message": "parse error"},
                    }),
                )
                .await?;
                continue;
            }
        };

        let jsonrpc_id = match request.get("id") {
            Some(Value::String(value)) => value.clone(),
            Some(Value::Number(value)) => value.to_string(),
            Some(other) => other.to_string(),
            None => continue,
        };

        if let Some(cached_result) = recent_calls
            .lock()
            .await
            .check(&dedupe_key, &jsonrpc_id)
            .cloned()
        {
            write_json_line(
                &writer,
                &json!({
                    "jsonrpc":"2.0",
                    "id": request.get("id").cloned().unwrap_or(Value::Null),
                    "result": cached_result,
                }),
            )
            .await?;
            continue;
        }

        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !dispatcher.contains(&method) {
            write_json_line(
                &writer,
                &json!({
                    "jsonrpc":"2.0",
                    "id": request.get("id").cloned().unwrap_or(Value::Null),
                    "error": {"code": -32601, "message": format!("method not found: {method}")},
                }),
            )
            .await?;
            continue;
        }

        let params = request.get("params").cloned().unwrap_or(Value::Null);
        tracing::info!(
            method = %method,
            worker_id = worker_id.as_deref().unwrap_or(""),
            "tool call"
        );
        let started_at = Instant::now();
        match dispatcher.dispatch(&method, params, &caller).await {
            Ok(result) => {
                tracing::info!(
                    method = %method,
                    worker_id = worker_id.as_deref().unwrap_or(""),
                    elapsed_ms = started_at.elapsed().as_millis() as u64,
                    "tool call finished"
                );
                recent_calls
                    .lock()
                    .await
                    .insert(&dedupe_key, &jsonrpc_id, result.clone());
                write_json_line(
                    &writer,
                    &json!({
                        "jsonrpc":"2.0",
                        "id": request.get("id").cloned().unwrap_or(Value::Null),
                        "result": result,
                    }),
                )
                .await?;
            }
            Err(error) => {
                tracing::error!(
                    method = %method,
                    worker_id = worker_id.as_deref().unwrap_or(""),
                    error = %error,
                    elapsed_ms = started_at.elapsed().as_millis() as u64,
                    "tool call failed"
                );
                write_json_line(
                    &writer,
                    &json!({
                        "jsonrpc":"2.0",
                        "id": request.get("id").cloned().unwrap_or(Value::Null),
                        "error": error.to_jsonrpc_error(),
                    }),
                )
                .await?;
            }
        }
    }

    {
        let mut connections = active_connections.write().await;
        connections.remove(&connection_id);
    }
    if let Some(worker_id) = worker_id.as_ref() {
        push_registry.write().await.deregister(worker_id);
        let mut session = match storage.load_session()? {
            Some(session) => session,
            None => return Ok(()),
        };
        if let Some(worker) = session.workers.get_mut(worker_id.as_str()) {
            worker.mcp_connected = false;
            storage.save_session(&session)?;
        }
    }
    tracing::info!(
        peer = %connection_id,
        worker_id = worker_id.as_deref().unwrap_or(""),
        role = ?role,
        "disconnected"
    );
    Ok(())
}

async fn perform_hello(
    connection_id: &str,
    request: &Value,
    storage: &Arc<Storage>,
    dispatcher: &Arc<Dispatcher>,
    push_registry: &Arc<RwLock<PushRegistry>>,
    active_connections: &Arc<RwLock<HashMap<String, ConnectedClient>>>,
    writer: Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
) -> Result<(ConnectedClient, Value), Value> {
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let role = match params.get("role").and_then(Value::as_str) {
        Some("manager") => WorkerRole::Manager,
        Some("worker") => WorkerRole::Worker,
        Some(other) => {
            return Err(jsonrpc_error(
                request.get("id").cloned().unwrap_or(Value::Null),
                McpError::ValidationFailed {
                    field: "role".to_string(),
                    reason: format!("unsupported role: {other}"),
                },
            ))
        }
        None => {
            return Err(jsonrpc_error(
                request.get("id").cloned().unwrap_or(Value::Null),
                McpError::ValidationFailed {
                    field: "role".to_string(),
                    reason: "missing".to_string(),
                },
            ))
        }
    };

    let session = match storage.load_session() {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Err(jsonrpc_error(
                request.get("id").cloned().unwrap_or(Value::Null),
                McpError::InvalidState {
                    message: "no active session".to_string(),
                },
            ))
        }
        Err(error) => {
            return Err(json!({
                "jsonrpc":"2.0",
                "id": request.get("id").cloned().unwrap_or(Value::Null),
                "error": {"code": -32603, "message": error.to_string()},
            }))
        }
    };

    let session_id = match params.get("session_id").and_then(Value::as_str) {
        Some(value) if value == session.id => value.to_string(),
        Some(_) => {
            return Err(jsonrpc_error(
                request.get("id").cloned().unwrap_or(Value::Null),
                McpError::ValidationFailed {
                    field: "session_id".to_string(),
                    reason: "mismatch".to_string(),
                },
            ))
        }
        None => {
            return Err(jsonrpc_error(
                request.get("id").cloned().unwrap_or(Value::Null),
                McpError::ValidationFailed {
                    field: "session_id".to_string(),
                    reason: "missing".to_string(),
                },
            ))
        }
    };

    let worker_id = match role {
        WorkerRole::Manager => {
            let manager_id = params
                .get("worker_id")
                .and_then(Value::as_str)
                .map(|value| value.to_string())
                .or_else(|| {
                    session
                        .manager_id
                        .clone()
                        .filter(|candidate| session.workers.contains_key(candidate))
                });

            if let Some(manager_id) = manager_id {
                if session.manager_id.as_deref() != Some(manager_id.as_str()) {
                    return Err(jsonrpc_error(
                        request.get("id").cloned().unwrap_or(Value::Null),
                        McpError::ValidationFailed {
                            field: "worker_id".to_string(),
                            reason: "manager mismatch".to_string(),
                        },
                    ));
                }
                if !session.workers.contains_key(&manager_id) {
                    return Err(jsonrpc_error(
                        request.get("id").cloned().unwrap_or(Value::Null),
                        McpError::WorkerNotFound(manager_id),
                    ));
                }
                Some(manager_id)
            } else {
                None
            }
        }
        WorkerRole::Worker => {
            let worker_id = params
                .get("worker_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    jsonrpc_error(
                        request.get("id").cloned().unwrap_or(Value::Null),
                        McpError::ValidationFailed {
                            field: "worker_id".to_string(),
                            reason: "missing".to_string(),
                        },
                    )
                })?
                .to_string();

            if !session.workers.contains_key(&worker_id) {
                return Err(jsonrpc_error(
                    request.get("id").cloned().unwrap_or(Value::Null),
                    McpError::WorkerNotFound(worker_id),
                ));
            }

            Some(worker_id)
        }
    };

    let mut updated_session = match storage.load_session() {
        Ok(Some(current)) => current,
        Ok(None) => session.clone(),
        Err(error) => {
            return Err(json!({
                "jsonrpc":"2.0",
                "id": request.get("id").cloned().unwrap_or(Value::Null),
                "error": {"code": -32603, "message": error.to_string()},
            }))
        }
    };
    if let Some(worker_id) = &worker_id {
        if let Some(worker) = updated_session.workers.get_mut(worker_id) {
            worker.mcp_connected = true;
            worker.status = if worker.job_id.is_some() {
                WorkerStatus::Running
            } else {
                WorkerStatus::Idle
            };
        }
        if let Err(error) = storage.save_session(&updated_session) {
            return Err(json!({
                "jsonrpc":"2.0",
                "id": request.get("id").cloned().unwrap_or(Value::Null),
                "error": {"code": -32603, "message": error.to_string()},
            }));
        }
    }

    if let Some(worker_id) = &worker_id {
        push_registry.write().await.deregister(worker_id);

        {
            let mut connections = active_connections.write().await;
            let stale = connections
                .iter()
                .find(|(_, client)| client.worker_id.as_deref() == Some(worker_id.as_str()))
                .map(|(id, _)| id.clone());
            if let Some(stale_connection_id) = stale {
                connections.remove(&stale_connection_id);
            }
        }

        push_registry
            .write()
            .await
            .register_shared(worker_id, Arc::clone(&writer));
    }

    let caller = ConnectedClient {
        connection_id: connection_id.to_string(),
        worker_id,
        role: role.clone(),
        session_id,
    };
    active_connections
        .write()
        .await
        .insert(caller.connection_id.clone(), caller.clone());

    let notification_mode = match NotificationMode::Poll {
        NotificationMode::Push => "push",
        NotificationMode::Poll => "poll",
    };
    let response = json!({
        "jsonrpc":"2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "result": {
            "tools": dispatcher.tools_for_role(&role),
            "notification_mode": notification_mode,
            "queued_notifications": [],
        }
    });

    Ok((caller, response))
}

fn json_error_message(value: &Value) -> String {
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .to_string()
}

async fn write_json_line(
    writer: &Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
    value: &Value,
) -> Result<(), ServerError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut writer = writer.lock().await;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

fn jsonrpc_error(id: Value, error: McpError) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id": id,
        "error": error.to_jsonrpc_error(),
    })
}

#[cfg(test)]
mod tests {
    use super::{ConnectedClient, McpServer};
    use crate::mcp::dispatcher::Dispatcher;
    use crate::mcp::error::McpError;
    use crate::storage::Storage;
    use crate::types::{GitStrategy, NotificationMode, Session, Worker, WorkerRole, WorkerStatus};
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    struct CountingTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl crate::mcp::dispatcher::Tool for CountingTool {
        fn name(&self) -> &str {
            "counting.ping"
        }

        fn allowed_roles(&self) -> &[WorkerRole] {
            &[WorkerRole::Worker]
        }

        async fn call(&self, _params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Value::Null)
        }
    }

    fn sample_session() -> Session {
        let manager = Worker {
            id: "w0".to_string(),
            index: 0,
            provider: "codex".to_string(),
            role: WorkerRole::Manager,
            status: WorkerStatus::Starting,
            job_id: None,
            pid: Some(100),
            pane_id: "%0".to_string(),
            mcp_connected: false,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: None,
            last_progress: None,
            permissions: vec![],
            started_at: chrono::Utc::now(),
        };
        let worker = Worker {
            id: "w1".to_string(),
            index: 1,
            provider: "codex".to_string(),
            role: WorkerRole::Worker,
            status: WorkerStatus::Idle,
            job_id: None,
            pid: None,
            pane_id: "%1".to_string(),
            mcp_connected: false,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: None,
            last_progress: None,
            permissions: vec![],
            started_at: chrono::Utc::now(),
        };
        let worker_two = Worker {
            id: "w2".to_string(),
            index: 2,
            ..worker.clone()
        };

        Session {
            id: "sess_abc123".to_string(),
            workspace_path: "/tmp/workspace".to_string(),
            workspace_hash: "test-hash".to_string(),
            manager_id: Some("w0".to_string()),
            workers: [
                (manager.id.clone(), manager),
                (worker.id.clone(), worker),
                (worker_two.id.clone(), worker_two),
            ]
            .into_iter()
            .collect::<HashMap<_, _>>(),
            jobs: HashMap::new(),
            notes: vec![],
            worker_seq: 3,
            job_seq: 1,
            request_seq: 1,
            git_strategy: GitStrategy::None,
            available_providers: vec!["codex".to_string()],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    async fn start_server(workspace_hash: &str) -> (tempfile::TempDir, Arc<Storage>, McpServer) {
        let temp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(temp.path()).unwrap());
        storage.save_session(&sample_session()).unwrap();
        let server = McpServer::new(workspace_hash, Arc::clone(&storage));
        server.start().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (temp, storage, server)
    }

    async fn connect(workspace_hash: &str) -> BufReader<UnixStream> {
        let stream = UnixStream::connect(format!("/tmp/kingdom/{workspace_hash}.sock"))
            .await
            .unwrap();
        BufReader::new(stream)
    }

    async fn write_line(reader: &mut BufReader<UnixStream>, value: Value) {
        let stream = reader.get_mut();
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).await.unwrap();
        stream.flush().await.unwrap();
    }

    async fn read_line(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn hello_as_manager_returns_tools_and_poll_mode() {
        let (_temp, _storage, server) = start_server("m2-manager-hello").await;
        let mut client = connect("m2-manager-hello").await;

        write_line(
            &mut client,
            json!({"jsonrpc":"2.0","id":"init","method":"kingdom.hello","params":{"role":"manager","session_id":"sess_abc123"}}),
        )
        .await;

        let response = read_line(&mut client).await;
        assert_eq!(response["result"]["notification_mode"], "poll");
        assert_eq!(response["result"]["queued_notifications"], json!([]));
        assert!(response["result"]["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("worker.create")));

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn manager_hello_marks_manager_worker_connected() {
        let (_temp, storage, server) = start_server("m2-manager-worker-hello").await;
        let mut client = connect("m2-manager-worker-hello").await;

        write_line(
            &mut client,
            json!({"jsonrpc":"2.0","id":"init","method":"kingdom.hello","params":{"role":"manager","session_id":"sess_abc123","worker_id":"w0"}}),
        )
        .await;

        let response = read_line(&mut client).await;
        assert!(response.get("result").is_some());
        let session = storage.load_session().unwrap().unwrap();
        assert!(session.workers["w0"].mcp_connected);
        assert_eq!(session.workers["w0"].status, WorkerStatus::Idle);

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn wrong_session_id_returns_error_and_disconnects() {
        let (_temp, _storage, server) = start_server("m2-wrong-session").await;
        let mut client = connect("m2-wrong-session").await;

        write_line(
            &mut client,
            json!({"jsonrpc":"2.0","id":"init","method":"kingdom.hello","params":{"role":"manager","session_id":"sess_wrong"}}),
        )
        .await;

        let response = read_line(&mut client).await;
        assert_eq!(response["error"]["code"], -32004);

        let mut line = String::new();
        let bytes = client.read_line(&mut line).await.unwrap();
        assert_eq!(bytes, 0);

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn worker_hello_with_unknown_worker_id_returns_error() {
        let (_temp, _storage, server) = start_server("m2-worker-missing").await;
        let mut client = connect("m2-worker-missing").await;

        write_line(
            &mut client,
            json!({"jsonrpc":"2.0","id":"init","method":"kingdom.hello","params":{"role":"worker","session_id":"sess_abc123","worker_id":"w999"}}),
        )
        .await;

        let response = read_line(&mut client).await;
        assert_eq!(response["error"]["code"], -32002);

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn worker_hello_marks_worker_connected_and_returns_worker_tools() {
        let (_temp, storage, server) = start_server("m2-worker-hello-tools").await;
        let mut client = connect("m2-worker-hello-tools").await;

        write_line(
            &mut client,
            json!({"jsonrpc":"2.0","id":"init","method":"kingdom.hello","params":{"role":"worker","session_id":"sess_abc123","worker_id":"w1"}}),
        )
        .await;

        let response = read_line(&mut client).await;
        let tools = response["result"]["tools"].as_array().unwrap();
        assert!(tools.contains(&json!("job.progress")));
        assert!(!tools.contains(&json!("worker.create")));

        let session = storage.load_session().unwrap().unwrap();
        assert!(session.workers["w1"].mcp_connected);
        assert_eq!(session.workers["w1"].status, WorkerStatus::Idle);

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn stub_tool_returns_null_after_hello() {
        let (_temp, _storage, server) = start_server("m2-stub-tool").await;
        let mut client = connect("m2-stub-tool").await;

        write_line(
            &mut client,
            json!({"jsonrpc":"2.0","id":"init","method":"kingdom.hello","params":{"role":"manager","session_id":"sess_abc123"}}),
        )
        .await;
        let _ = read_line(&mut client).await;

        write_line(
            &mut client,
            json!({"jsonrpc":"2.0","id":"1","method":"worker.create","params":{}}),
        )
        .await;
        let response = read_line(&mut client).await;
        assert_eq!(response["result"], Value::Null);

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn worker_calling_manager_tool_gets_unauthorized() {
        let (_temp, _storage, server) = start_server("m2-unauthorized").await;
        let mut client = connect("m2-unauthorized").await;

        write_line(
            &mut client,
            json!({"jsonrpc":"2.0","id":"init","method":"kingdom.hello","params":{"role":"worker","session_id":"sess_abc123","worker_id":"w1"}}),
        )
        .await;
        let _ = read_line(&mut client).await;

        write_line(
            &mut client,
            json!({"jsonrpc":"2.0","id":"1","method":"worker.create","params":{}}),
        )
        .await;
        let response = read_line(&mut client).await;
        assert_eq!(response["error"]["code"], -32001);

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn replay_returns_cached_response_for_same_worker_and_id() {
        let temp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(temp.path()).unwrap());
        storage.save_session(&sample_session()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut dispatcher = Dispatcher::new();
        dispatcher.register(Box::new(CountingTool {
            calls: Arc::clone(&calls),
        }));
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let server = McpServer {
            workspace_hash: "m2-replay".to_string(),
            storage,
            dispatcher: Arc::new(dispatcher),
            push_registry: Arc::new(tokio::sync::RwLock::new(
                crate::mcp::push::PushRegistry::new(),
            )),
            recent_calls: Arc::new(tokio::sync::Mutex::new(
                crate::mcp::replay::RecentCalls::new(),
            )),
            active_connections: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            shutdown,
            listener_task: Arc::new(tokio::sync::Mutex::new(None)),
        };
        server.start().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = connect("m2-replay").await;
        write_line(
            &mut client,
            json!({"jsonrpc":"2.0","id":"init","method":"kingdom.hello","params":{"role":"worker","session_id":"sess_abc123","worker_id":"w1"}}),
        )
        .await;
        let _ = read_line(&mut client).await;

        let request = json!({"jsonrpc":"2.0","id":"42","method":"counting.ping","params":{}});
        write_line(&mut client, request.clone()).await;
        let first = read_line(&mut client).await;
        write_line(&mut client, request).await;
        let second = read_line(&mut client).await;

        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn two_clients_can_connect_with_same_session_id() {
        let (_temp, _storage, server) = start_server("m2-multi-client").await;
        let (mut first, mut second) =
            tokio::join!(connect("m2-multi-client"), connect("m2-multi-client"));

        let first_hello = write_line(
            &mut first,
            json!({"jsonrpc":"2.0","id":"init-1","method":"kingdom.hello","params":{"role":"worker","session_id":"sess_abc123","worker_id":"w1"}}),
        );
        let second_hello = write_line(
            &mut second,
            json!({"jsonrpc":"2.0","id":"init-2","method":"kingdom.hello","params":{"role":"worker","session_id":"sess_abc123","worker_id":"w2"}}),
        );
        tokio::join!(first_hello, second_hello);

        let first_response = read_line(&mut first).await;
        let second_response = read_line(&mut second).await;

        assert_eq!(first_response["result"]["notification_mode"], "poll");
        assert_eq!(second_response["result"]["notification_mode"], "poll");

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn stop_removes_socket_file() {
        let (_temp, _storage, server) = start_server("m2-stop-cleanup").await;
        let socket_path = std::path::PathBuf::from("/tmp/kingdom/m2-stop-cleanup.sock");
        assert!(socket_path.exists());
        server.stop().await.unwrap();
        assert!(!socket_path.exists());
    }
}
