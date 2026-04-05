use super::{default_tree_path, resolve_workspace_path};
use crate::mcp::dispatcher::{Dispatcher, Tool};
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::server::ConnectedClient;
use crate::mcp::tools::manager::{append_action_log, load_session, parse_params, storage_error};
use crate::storage::Storage;
use crate::types::WorkerRole;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::sync::Arc;
use tokio::sync::RwLock;

pub fn register(dispatcher: &mut Dispatcher, storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) {
    dispatcher.register(Box::new(FileReadTool::new(Arc::clone(&storage), Arc::clone(&push))));
    dispatcher.register(Box::new(WorkspaceTreeTool::new(storage, push)));
}

#[derive(Deserialize)]
struct FileReadParams {
    path: String,
    lines: Option<String>,
    symbol: Option<String>,
}

pub struct FileReadTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl FileReadTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file.read"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<FileReadParams>(params)?;
        let session = load_session(&self.storage)?;
        let resolved = resolve_workspace_path(&session.workspace_path, &params.path)?;
        let content = fs::read_to_string(&resolved).map_err(storage_error)?;
        if let Some(symbol) = params.symbol {
            append_action_log(
                &self.storage,
                caller,
                "file.read.symbol_fallback",
                json!({ "path": params.path, "symbol": symbol }),
                None,
            )?;
            return Ok(Value::String(format!(
                "# [symbol lookup not supported in M4, falling back to full file read]\n{}",
                content
            )));
        }
        let lines = content.lines().collect::<Vec<_>>();
        let selected = if let Some(range) = params.lines {
            let (start, end) = parse_line_range(&range)?;
            lines
                .into_iter()
                .skip(start.saturating_sub(1))
                .take(end.saturating_sub(start) + 1)
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            lines.into_iter().take(200).collect::<Vec<_>>().join("\n")
        };
        Ok(Value::String(selected))
    }
}

fn parse_line_range(range: &str) -> Result<(usize, usize), McpError> {
    let mut parts = range.split('-');
    let start = parts
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| McpError::ValidationFailed {
            field: "lines".to_string(),
            reason: "invalid line range".to_string(),
        })?;
    let end = parts
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| McpError::ValidationFailed {
            field: "lines".to_string(),
            reason: "invalid line range".to_string(),
        })?;
    Ok((start, end))
}

#[derive(Deserialize)]
struct WorkspaceTreeParams {
    path: Option<String>,
}

pub struct WorkspaceTreeTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkspaceTreeTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkspaceTreeTool {
    fn name(&self) -> &str {
        "workspace.tree"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<WorkspaceTreeParams>(params)?;
        let session = load_session(&self.storage)?;
        let path = default_tree_path(&session.workspace_path, params.path)?;
        let output = std::process::Command::new("find")
            .arg(path)
            .args(["-maxdepth", "3", "-not", "-path", "*/.*"])
            .output()
            .map_err(storage_error)?;
        if !output.status.success() {
            return Err(storage_error(String::from_utf8_lossy(&output.stderr).trim()));
        }
        Ok(Value::String(String::from_utf8_lossy(&output.stdout).to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::dispatcher::Tool;
    use crate::mcp::tools::worker::testsupport::setup_worker;
    use serde_json::json;

    #[tokio::test]
    async fn file_read_blocks_path_traversal() {
        let (_temp, storage, push, _notifications, _health, _awaiters, caller) = setup_worker();
        let tool = FileReadTool::new(storage, push);
        let err = tool
            .call(json!({"path":"../../../etc/passwd"}), &caller)
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ValidationFailed { field, .. } if field == "path"));
    }

    #[tokio::test]
    async fn file_read_supports_full_lines_and_symbol_fallback() {
        let (temp, storage, push, _notifications, _health, _awaiters, caller) = setup_worker();
        std::fs::write(temp.path().join("sample.txt"), "1\n2\n3\n4\n").unwrap();
        let tool = FileReadTool::new(Arc::clone(&storage), Arc::clone(&push));
        let full = tool.call(json!({"path":"sample.txt"}), &caller).await.unwrap();
        assert!(full.as_str().unwrap().contains("1\n2\n3\n4"));
        let lines = tool
            .call(json!({"path":"sample.txt","lines":"1-3"}), &caller)
            .await
            .unwrap();
        assert_eq!(lines, Value::String("1\n2\n3".to_string()));
        let symbol = tool
            .call(json!({"path":"sample.txt","symbol":"foo"}), &caller)
            .await
            .unwrap();
        assert!(symbol.as_str().unwrap().starts_with("# [symbol lookup not supported in M4"));
    }
}
