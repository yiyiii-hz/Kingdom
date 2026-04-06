//! kingdom-bridge: adapts standard MCP (stdio) to Kingdom's custom JSON-RPC protocol.
//!
//! Usage: kingdom-bridge <socket_path> <storage_root>
//!
//! Env vars (set by MCP config):
//!   KINGDOM_WORKER_ID  - worker id (e.g. "w0", "w1")
//!   KINGDOM_ROLE       - "manager" or "worker"
//!
//! Protocol translation:
//!   claude → initialize/tools/list/tools/call (standard MCP) → kingdom-bridge
//!   kingdom-bridge → kingdom.hello + method calls (kingdom custom) → kingdom socket

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use std::{
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

type PendingMap = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;
type SharedWriter = Arc<Mutex<Option<BufWriter<UnixStream>>>>;
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
    Connected,
    Disconnected,
    Reconnecting,
}

struct BridgeRuntime {
    state: Arc<Mutex<ConnectionState>>,
    writer: SharedWriter,
    tool_names: Arc<Mutex<Vec<String>>>,
}

fn log(msg: &str) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{ts}] {msg}\n");
    if let Some(path) = LOG_PATH.get() {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
    }
    eprintln!("kingdom-bridge: {msg}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let socket_path = args
        .get(1)
        .cloned()
        .or_else(|| std::env::var("KINGDOM_SOCKET").ok())
        .map(normalize_socket_path)
        .unwrap_or_else(|| {
            log("socket path required (arg 1 or KINGDOM_SOCKET)");
            std::process::exit(1);
        });

    let storage_root = args
        .get(2)
        .cloned()
        .or_else(|| std::env::var("KINGDOM_STORAGE").ok())
        .unwrap_or_else(|| {
            log("storage root required (arg 2 or KINGDOM_STORAGE)");
            std::process::exit(1);
        });

    let worker_id = std::env::var("KINGDOM_WORKER_ID").unwrap_or_default();
    let role = std::env::var("KINGDOM_ROLE").unwrap_or_else(|_| "worker".to_string());
    init_log_path(&storage_root, &worker_id);

    log(&format!("starting: args={:?}", &args[1..]));

    log(&format!(
        "role={role} worker_id={worker_id} socket={socket_path}"
    ));

    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let (push_tx, push_rx) = mpsc::channel();
    let runtime = BridgeRuntime {
        state: Arc::new(Mutex::new(ConnectionState::Disconnected)),
        writer: Arc::new(Mutex::new(None)),
        tool_names: Arc::new(Mutex::new(Vec::new())),
    };

    if let Err(e) = reconnect_to_kingdom(
        &socket_path,
        &storage_root,
        &role,
        &worker_id,
        &runtime,
        &pending,
        &push_tx,
        Duration::from_secs(5),
    ) {
        log(&format!("initial connect failed: {e}"));
        std::process::exit(1);
    }

    let socket_path_for_thread = socket_path.clone();
    let storage_root_for_thread = storage_root.clone();
    let role_for_thread = role.clone();
    let worker_id_for_thread = worker_id.clone();
    let pending_for_thread = Arc::clone(&pending);
    let push_tx_for_thread = push_tx.clone();
    let state_for_thread = Arc::clone(&runtime.state);
    let writer_for_thread = Arc::clone(&runtime.writer);
    let tools_for_thread = Arc::clone(&runtime.tool_names);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        if *state_for_thread.lock().unwrap() == ConnectionState::Connected {
            continue;
        }
        let thread_runtime = BridgeRuntime {
            state: Arc::clone(&state_for_thread),
            writer: Arc::clone(&writer_for_thread),
            tool_names: Arc::clone(&tools_for_thread),
        };
        if let Err(e) = reconnect_to_kingdom(
            &socket_path_for_thread,
            &storage_root_for_thread,
            &role_for_thread,
            &worker_id_for_thread,
            &thread_runtime,
            &pending_for_thread,
            &push_tx_for_thread,
            Duration::from_secs(30),
        ) {
            log(&format!("reconnect failed: {e}"));
        }
    });

    // Serve standard MCP on stdio
    run_mcp_server(&runtime, &pending, &push_rx);
}

fn init_log_path(storage_root: &str, worker_id: &str) {
    let path = bridge_log_path(Path::new(storage_root), worker_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = LOG_PATH.set(path);
}

fn normalize_socket_path(socket: String) -> String {
    socket
        .strip_prefix("UNIX-CONNECT:")
        .unwrap_or(&socket)
        .to_string()
}

fn bridge_log_path(storage_root: &Path, worker_id: &str) -> PathBuf {
    let worker_id = if worker_id.is_empty() {
        "unknown"
    } else {
        worker_id
    };
    storage_root
        .join("logs")
        .join(format!("bridge-{worker_id}.log"))
}

/// Read session_id from <storage_root>/state.json
fn read_session_id(storage_root: &str) -> Result<String, String> {
    let path = std::path::Path::new(storage_root).join("state.json");
    let data = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let v: Value = serde_json::from_slice(&data).map_err(|e| format!("parse state.json: {e}"))?;
    // Session is serialized with `#[serde(rename = "session_id")]`
    v["session_id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "state.json has no 'session_id' field".to_string())
}

/// Background thread: reads from kingdom socket and routes responses to pending callers.
/// Push notifications (no id or null id) are forwarded through `push_tx`.
fn kingdom_reader_loop(
    mut reader: BufReader<UnixStream>,
    pending: PendingMap,
    push_tx: mpsc::Sender<Value>,
) -> std::io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "kingdom closed connection",
                ))
            }
            Err(error) => return Err(error),
            Ok(_) => {}
        }

        let msg: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Route by string id
        let id = match msg.get("id") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => {
                let _ = push_tx.send(msg);
                continue;
            }
        };

        if let Some(tx) = pending.lock().unwrap().remove(&id) {
            let _ = tx.send(msg);
        }
    }
}

/// Main MCP server loop: reads JSON-RPC from stdin, writes to stdout.
fn run_mcp_server(runtime: &BridgeRuntime, pending: &PendingMap, push_rx: &mpsc::Receiver<Value>) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut call_counter: u64 = 0;

    for line in stdin.lock().lines() {
        drain_push_notifications(&mut stdout, push_rx);
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let method = request["method"].as_str().unwrap_or_default().to_string();
        let id = request.get("id").cloned();

        // Notifications have no id — just ignore them.
        if id.is_none() && method.starts_with("notifications/") {
            continue;
        }

        let id_val = id.unwrap_or(Value::Null);

        let response = match method.as_str() {
            "initialize" => handle_initialize(&id_val),
            "ping" => json!({"jsonrpc":"2.0","id":id_val,"result":{}}),
            "tools/list" => {
                let tool_names = runtime.tool_names.lock().unwrap().clone();
                handle_tools_list(&id_val, &tool_names)
            }
            "tools/call" => {
                call_counter += 1;
                let call_id = format!("bridge-call-{call_counter}");
                handle_tools_call(&id_val, &request, &call_id, runtime, pending)
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id_val,
                "error": {"code": -32601, "message": format!("method not found: {method}")}
            }),
        };

        if let Err(e) = write_mcp_response(&mut stdout, &response) {
            eprintln!("kingdom-bridge: write to stdout failed: {e}");
            break;
        }
    }
}

fn handle_initialize(id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "kingdom",
                "version": "0.1.0"
            }
        }
    })
}

fn handle_tools_list(id: &Value, tool_names: &[String]) -> Value {
    let tools: Vec<Value> = tool_names.iter().map(|name| tool_schema(name)).collect();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": tools
        }
    })
}

fn handle_tools_call(
    mcp_id: &Value,
    request: &Value,
    call_id: &str,
    runtime: &BridgeRuntime,
    pending: &PendingMap,
) -> Value {
    if *runtime.state.lock().unwrap() != ConnectionState::Connected {
        return reconnecting_response(mcp_id);
    }

    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let tool_name = params["name"].as_str().unwrap_or_default().to_string();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Register pending channel before sending
    let (tx, rx) = mpsc::channel();
    pending.lock().unwrap().insert(call_id.to_string(), tx);

    // Forward to kingdom
    let kingdom_req = json!({
        "jsonrpc": "2.0",
        "id": call_id,
        "method": tool_name,
        "params": arguments
    });

    if let Err(e) = write_kingdom(&runtime.writer, &kingdom_req) {
        pending.lock().unwrap().remove(call_id);
        mark_disconnected(runtime, pending);
        log(&format!("kingdom write error: {e}"));
        return reconnecting_response(mcp_id);
    }

    // Wait for kingdom response
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(resp) => {
            if let Some(result) = resp.get("result") {
                let text = serde_json::to_string_pretty(result).unwrap_or_default();
                json!({
                    "jsonrpc": "2.0",
                    "id": mcp_id,
                    "result": {
                        "content": [{"type": "text", "text": text}]
                    }
                })
            } else if let Some(err) = resp.get("error") {
                let msg = err["message"].as_str().unwrap_or("unknown error");
                json!({
                    "jsonrpc": "2.0",
                    "id": mcp_id,
                    "result": {
                        "content": [{"type": "text", "text": format!("Error: {msg}")}],
                        "isError": true
                    }
                })
            } else {
                error_response(mcp_id, "kingdom returned empty response")
            }
        }
        Err(_) => {
            pending.lock().unwrap().remove(call_id);
            error_response(mcp_id, "timeout waiting for kingdom response")
        }
    }
}

fn reconnecting_response(id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": "Kingdom reconnecting, please retry"}],
            "isError": true
        }
    })
}

fn error_response(id: &Value, msg: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": -32603, "message": msg}
    })
}

fn write_kingdom(writer: &SharedWriter, value: &Value) -> std::io::Result<()> {
    let mut guard = writer.lock().unwrap();
    let w = guard.as_mut().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotConnected, "kingdom not connected")
    })?;
    let mut bytes = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    w.write_all(&bytes)?;
    w.flush()
}

fn write_mcp_response<W: Write>(writer: &mut W, value: &Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

/// Returns a standard MCP tool description for a kingdom tool name.
/// Uses a generic inputSchema that accepts any JSON object.
fn tool_schema(name: &str) -> Value {
    let description = tool_description(name);
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "additionalProperties": true
        }
    })
}

fn tool_description(name: &str) -> &'static str {
    match name {
        // Manager tools
        "workspace.log" => "Log a message to the workspace action log",
        "workspace.note" => "Add a note to the workspace session",
        "workspace.notes" => "Get all workspace session notes",
        "workspace.status" => "Get the current workspace status (workers, jobs, session)",
        "worker.create" => "Create a new worker with a given provider and role",
        "worker.assign" => "Assign a job to a worker",
        "worker.release" => "Release a worker from its current job",
        "worker.swap" => "Swap a worker to a different AI provider",
        "worker.grant" => "Grant a permission to a worker",
        "worker.revoke" => "Revoke a permission from a worker",
        "job.create" => "Create a new job and optionally assign it to a worker",
        "job.cancel" => "Cancel a running or waiting job",
        "job.keep_waiting" => "Keep a job in waiting state (extend timeout)",
        "job.update" => "Update job metadata or description",
        "job.respond" => "Respond to a job request from a worker",
        "job.result" => "Get the result of a completed job",
        "job.list_all" => "List all jobs in the current session",
        "job.status" => "Get the status of a specific job",
        "failover.confirm" => "Confirm a failover to a new provider",
        "failover.cancel" => "Cancel a pending failover",
        // Worker tools
        "job.progress" => "Report progress on the current job (sends heartbeat)",
        "job.complete" => "Mark the current job as completed successfully",
        "job.fail" => "Mark the current job as failed with a reason",
        "job.cancelled" => "Acknowledge that the current job was cancelled",
        "job.checkpoint" => "Save a checkpoint of current job state for recovery",
        "job.request" => "Request information or approval from the manager",
        "job.request_status" => "Get the status of a pending job request",
        "file.read" => "Read a file from the workspace",
        "workspace.tree" => "Get the directory tree of the workspace",
        "git.log" => "Get git log for the workspace repository",
        "git.diff" => "Get git diff for the workspace repository",
        "context.ping" => "Send a heartbeat ping to keep the connection alive",
        "context.checkpoint_defer" => "Defer context checkpoint to avoid interruption",
        "subtask.create" => "Create a subtask within the current job",
        "worker.notify" => "Send a notification to another worker",
        _ => "Kingdom tool",
    }
}

fn hello_request(role: &str, session_id: &str, worker_id: &str) -> Value {
    let worker_id_val = if worker_id.is_empty() {
        Value::Null
    } else {
        Value::String(worker_id.to_string())
    };
    json!({
        "jsonrpc": "2.0",
        "id": "bridge-hello",
        "method": "kingdom.hello",
        "params": {
            "role": role,
            "session_id": session_id,
            "worker_id": worker_id_val,
        }
    })
}

fn connect_with_retry(socket_path: &str, timeout: Duration) -> Result<UnixStream, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match UnixStream::connect(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!("connect to {socket_path} failed: {error}"));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn reconnect_to_kingdom(
    socket_path: &str,
    storage_root: &str,
    role: &str,
    worker_id: &str,
    runtime: &BridgeRuntime,
    pending: &PendingMap,
    push_tx: &mpsc::Sender<Value>,
    timeout: Duration,
) -> Result<(), String> {
    *runtime.state.lock().unwrap() = ConnectionState::Reconnecting;
    wait_for_socket(socket_path, timeout)?;

    let session_id = read_session_id(storage_root)?;
    log(&format!("session_id={session_id}"));

    let stream = connect_with_retry(socket_path, timeout)?;
    log("connected to kingdom socket");
    let stream_read = stream
        .try_clone()
        .map_err(|e| format!("clone stream failed: {e}"))?;
    let mut reader = BufReader::new(stream_read);
    let writer = Arc::new(Mutex::new(Some(BufWriter::new(stream))));

    let hello = hello_request(role, &session_id, worker_id);
    write_kingdom(&writer, &hello).map_err(|e| format!("failed to send hello: {e}"))?;

    let mut hello_line = String::new();
    match reader.read_line(&mut hello_line) {
        Ok(0) => return Err("kingdom closed connection after hello".to_string()),
        Err(error) => return Err(format!("read hello response failed: {error}")),
        Ok(_) => {}
    }

    let hello_resp: Value =
        serde_json::from_str(&hello_line).map_err(|e| format!("bad hello response json: {e}"))?;
    if hello_resp.get("error").is_some() {
        let msg = hello_resp["error"]["message"].as_str().unwrap_or("unknown");
        return Err(format!(
            "kingdom.hello rejected: {msg} | full: {hello_resp}"
        ));
    }

    log(&format!(
        "hello ok, tools: {:?}",
        hello_resp["result"]["tools"]
    ));

    let tool_names = hello_resp["result"]["tools"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    *runtime.tool_names.lock().unwrap() = tool_names;
    *runtime.writer.lock().unwrap() = writer.lock().unwrap().take();
    *runtime.state.lock().unwrap() = ConnectionState::Connected;

    let reader_result = kingdom_reader_loop(reader, Arc::clone(pending), push_tx.clone());
    if let Err(error) = reader_result {
        log(&format!("kingdom reader stopped: {error}"));
    }
    mark_disconnected(runtime, pending);
    Err("connection lost".to_string())
}

fn wait_for_socket(socket_path: &str, timeout: Duration) -> Result<(), String> {
    let path = Path::new(socket_path);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "socket did not reappear within {}s",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn mark_disconnected(runtime: &BridgeRuntime, pending: &PendingMap) {
    *runtime.state.lock().unwrap() = ConnectionState::Disconnected;
    *runtime.writer.lock().unwrap() = None;
    let mut pending = pending.lock().unwrap();
    for (_, tx) in pending.drain() {
        let _ = tx.send(json!({
            "jsonrpc": "2.0",
            "error": {"message": "Kingdom reconnecting, please retry"}
        }));
    }
}

fn drain_push_notifications<W: Write>(writer: &mut W, push_rx: &mpsc::Receiver<Value>) {
    while let Ok(msg) = push_rx.try_recv() {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {
                "level": "info",
                "data": msg
            }
        });
        if write_mcp_response(writer, &notification).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::thread;

    #[test]
    fn tool_schema_has_required_fields() {
        let schema = tool_schema("job.progress");
        assert_eq!(schema["name"], "job.progress");
        assert!(!schema["description"].as_str().unwrap_or("").is_empty());
        assert_eq!(schema["inputSchema"]["type"], "object");
    }

    #[test]
    fn tool_schema_unknown_tool_has_generic_description() {
        let schema = tool_schema("unknown.tool");
        assert_eq!(schema["description"], "Kingdom tool");
    }

    #[test]
    fn read_session_id_parses_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let state = serde_json::json!({"session_id": "sess_test123", "workspace_path": "/tmp"});
        std::fs::write(dir.path().join("state.json"), state.to_string()).unwrap();
        let id = read_session_id(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(id, "sess_test123");
    }

    #[test]
    fn bridge_log_path_uses_storage_logs_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = bridge_log_path(dir.path(), "w0");
        assert_eq!(path, dir.path().join("logs/bridge-w0.log"));
    }

    #[test]
    fn normalize_socket_path_accepts_socat_prefix() {
        assert_eq!(
            normalize_socket_path("UNIX-CONNECT:/tmp/kingdom.sock".to_string()),
            "/tmp/kingdom.sock"
        );
    }

    #[test]
    fn worker_hello_request_includes_worker_id() {
        let hello = hello_request("worker", "sess_1", "w1");
        assert_eq!(hello["params"]["worker_id"], "w1");
    }

    #[test]
    fn kingdom_reader_loop_forwards_push_notifications() {
        let (server, client) = StdUnixStream::pair().unwrap();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (push_tx, push_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let reader = BufReader::new(server);
            let _ = kingdom_reader_loop(reader, pending, push_tx);
        });

        let mut client_writer = client.try_clone().unwrap();
        client_writer
            .write_all(br#"{"jsonrpc":"2.0","method":"job.assigned","params":{"job_id":"j1"}}"#)
            .unwrap();
        client_writer.write_all(b"\n").unwrap();
        drop(client_writer);
        drop(client);

        let msg = push_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(msg["method"], "job.assigned");
        handle.join().unwrap();
    }

    #[test]
    fn disconnected_tools_call_returns_retryable_error() {
        let runtime = BridgeRuntime {
            state: Arc::new(Mutex::new(ConnectionState::Reconnecting)),
            writer: Arc::new(Mutex::new(None)),
            tool_names: Arc::new(Mutex::new(Vec::new())),
        };
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let response = handle_tools_call(
            &json!("1"),
            &json!({"params":{"name":"job.progress","arguments":{}}}),
            "bridge-call-1",
            &runtime,
            &pending,
        );
        assert_eq!(
            response["result"]["content"][0]["text"],
            "Kingdom reconnecting, please retry"
        );
        assert_eq!(response["result"]["isError"], true);
    }

    #[test]
    fn drain_push_notifications_emits_mcp_notification() {
        let (tx, rx) = mpsc::channel();
        tx.send(json!({"method":"job.assigned","params":{"job_id":"j1"}}))
            .unwrap();
        let mut out = Vec::new();
        drain_push_notifications(&mut out, &rx);
        let text = String::from_utf8(out).unwrap();
        let value: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["method"], "notifications/message");
        assert_eq!(value["params"]["data"]["method"], "job.assigned");
    }
}
