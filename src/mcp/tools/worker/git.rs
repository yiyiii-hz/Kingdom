use crate::mcp::dispatcher::{Dispatcher, Tool};
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::server::ConnectedClient;
use crate::mcp::tools::manager::{load_session, parse_params, storage_error, to_value};
use crate::mcp::tools::worker::resolve_workspace_path;
use crate::storage::Storage;
use crate::types::{GitLogEntry, WorkerRole};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

pub fn register(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
) {
    dispatcher.register(Box::new(GitLogTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
    )));
    dispatcher.register(Box::new(GitDiffTool::new(storage, push)));
}

#[derive(Deserialize)]
struct GitLogParams {
    n: Option<u32>,
}

pub struct GitLogTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl GitLogTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for GitLogTool {
    fn name(&self) -> &str {
        "git.log"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<GitLogParams>(params)?;
        let session = load_session(&self.storage)?;
        let n = params.n.unwrap_or(10);
        let output = std::process::Command::new("git")
            .args([
                "-C",
                &session.workspace_path,
                "log",
                &format!("-{n}"),
                "--format=%H|||%an|||%ae|||%aI|||%s",
            ])
            .output()
            .map_err(storage_error)?;
        if !output.status.success() {
            return Err(storage_error(
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }
        let entries = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let parts = line.split("|||").collect::<Vec<_>>();
                if parts.len() != 5 {
                    return None;
                }
                let timestamp = DateTime::parse_from_rfc3339(parts[3])
                    .ok()?
                    .with_timezone(&Utc);
                Some(GitLogEntry {
                    hash: parts[0].to_string(),
                    author: parts[1].to_string(),
                    timestamp,
                    message: parts[4].to_string(),
                })
            })
            .collect::<Vec<_>>();
        to_value(&entries)
    }
}

#[derive(Deserialize)]
struct GitDiffParams {
    path: Option<String>,
}

pub struct GitDiffTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl GitDiffTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git.diff"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Worker]
    }

    async fn call(&self, params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<GitDiffParams>(params)?;
        let session = load_session(&self.storage)?;
        let mut cmd = std::process::Command::new("git");
        cmd.args(["-C", &session.workspace_path, "diff"]);
        if let Some(path) = params.path {
            let resolved = resolve_workspace_path(&session.workspace_path, &path)?;
            cmd.arg(resolved);
        }
        let output = cmd.output().map_err(storage_error)?;
        if !output.status.success() {
            return Err(storage_error(
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }
        Ok(Value::String(
            String::from_utf8_lossy(&output.stdout).to_string(),
        ))
    }
}
