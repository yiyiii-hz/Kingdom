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
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

type PendingMap = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

/// Write a timestamped line to /tmp/kingdom-bridge.log for debugging.
fn log(msg: &str) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{ts}] {msg}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/kingdom-bridge.log")
        .and_then(|mut f| f.write_all(line.as_bytes()));
    eprintln!("kingdom-bridge: {msg}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    log(&format!("starting: args={:?}", &args[1..]));

    let socket_path = args
        .get(1)
        .cloned()
        .or_else(|| std::env::var("KINGDOM_SOCKET").ok())
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

    log(&format!("role={role} worker_id={worker_id} socket={socket_path}"));

    // Read session_id from state.json
    let session_id = match read_session_id(&storage_root) {
        Ok(id) => {
            log(&format!("session_id={id}"));
            id
        }
        Err(e) => {
            log(&format!("failed to read session id: {e}"));
            std::process::exit(1);
        }
    };

    // Connect to kingdom unix socket (retry up to 5s in case daemon is still initializing)
    let stream = {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match UnixStream::connect(&socket_path) {
                Ok(s) => break s,
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        log(&format!("connect to {socket_path} failed: {e}"));
                        std::process::exit(1);
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    };

    log("connected to kingdom socket");

    let stream_read = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log(&format!("clone stream failed: {e}"));
            std::process::exit(1);
        }
    };

    let mut kingdom_reader = BufReader::new(stream_read);
    let kingdom_writer = Arc::new(Mutex::new(BufWriter::new(stream)));

    // Send kingdom.hello
    let worker_id_val = if worker_id.is_empty() {
        Value::Null
    } else {
        Value::String(worker_id.clone())
    };

    let hello = json!({
        "jsonrpc": "2.0",
        "id": "bridge-hello",
        "method": "kingdom.hello",
        "params": {
            "role": role,
            "session_id": session_id,
            "worker_id": worker_id_val,
        }
    });

    if let Err(e) = write_kingdom(&kingdom_writer, &hello) {
        log(&format!("failed to send hello: {e}"));
        std::process::exit(1);
    }

    // Read kingdom.hello response (synchronous, before starting background thread)
    let mut hello_line = String::new();
    match kingdom_reader.read_line(&mut hello_line) {
        Ok(0) => {
            log("kingdom closed connection after hello");
            std::process::exit(1);
        }
        Err(e) => {
            log(&format!("read hello response failed: {e}"));
            std::process::exit(1);
        }
        Ok(_) => {}
    }

    let hello_resp: Value = match serde_json::from_str(&hello_line) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("bad hello response json: {e}"));
            std::process::exit(1);
        }
    };

    if hello_resp.get("error").is_some() {
        let msg = hello_resp["error"]["message"]
            .as_str()
            .unwrap_or("unknown");
        log(&format!("kingdom.hello rejected: {msg} | full: {hello_resp}"));
        std::process::exit(1);
    }

    log(&format!("hello ok, tools: {:?}", hello_resp["result"]["tools"]));

    // Extract tool names from hello response
    let tool_names: Vec<String> = hello_resp["result"]["tools"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Start background thread to route kingdom responses by id
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let pending_for_thread = Arc::clone(&pending);
    std::thread::spawn(move || {
        kingdom_reader_loop(kingdom_reader, pending_for_thread);
    });

    // Serve standard MCP on stdio
    run_mcp_server(&tool_names, &kingdom_writer, &pending);
}

/// Read session_id from <storage_root>/state.json
fn read_session_id(storage_root: &str) -> Result<String, String> {
    let path = std::path::Path::new(storage_root).join("state.json");
    let data = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let v: Value =
        serde_json::from_slice(&data).map_err(|e| format!("parse state.json: {e}"))?;
    // Session is serialized with `#[serde(rename = "session_id")]`
    v["session_id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "state.json has no 'session_id' field".to_string())
}

/// Background thread: reads from kingdom socket and routes responses to pending callers.
/// Push notifications (no id or null id) are silently discarded.
fn kingdom_reader_loop(mut reader: BufReader<UnixStream>, pending: PendingMap) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // kingdom closed connection
            Err(_) => break,
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
            _ => continue, // push notification — discard
        };

        if let Some(tx) = pending.lock().unwrap().remove(&id) {
            let _ = tx.send(msg);
        }
    }
}

/// Main MCP server loop: reads JSON-RPC from stdin, writes to stdout.
fn run_mcp_server(
    tool_names: &[String],
    kingdom_writer: &Arc<Mutex<BufWriter<UnixStream>>>,
    pending: &PendingMap,
) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut call_counter: u64 = 0;

    for line in stdin.lock().lines() {
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
            "tools/list" => handle_tools_list(&id_val, tool_names),
            "tools/call" => {
                call_counter += 1;
                let call_id = format!("bridge-call-{call_counter}");
                handle_tools_call(&id_val, &request, &call_id, kingdom_writer, pending)
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
    kingdom_writer: &Arc<Mutex<BufWriter<UnixStream>>>,
    pending: &PendingMap,
) -> Value {
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let tool_name = params["name"].as_str().unwrap_or_default().to_string();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Register pending channel before sending
    let (tx, rx) = mpsc::channel();
    pending
        .lock()
        .unwrap()
        .insert(call_id.to_string(), tx);

    // Forward to kingdom
    let kingdom_req = json!({
        "jsonrpc": "2.0",
        "id": call_id,
        "method": tool_name,
        "params": arguments
    });

    if let Err(e) = write_kingdom(kingdom_writer, &kingdom_req) {
        pending.lock().unwrap().remove(call_id);
        return error_response(mcp_id, &format!("kingdom write error: {e}"));
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

fn error_response(id: &Value, msg: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": -32603, "message": msg}
    })
}

fn write_kingdom(
    writer: &Arc<Mutex<BufWriter<UnixStream>>>,
    value: &Value,
) -> std::io::Result<()> {
    let mut w = writer.lock().unwrap();
    let mut bytes = serde_json::to_vec(value).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    bytes.push(b'\n');
    w.write_all(&bytes)?;
    w.flush()
}

fn write_mcp_response<W: Write>(writer: &mut W, value: &Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
