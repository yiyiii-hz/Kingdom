use crate::mcp::error::McpError;
use crate::mcp::server::ConnectedClient;
use crate::types::WorkerRole;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn allowed_roles(&self) -> &[WorkerRole];
    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError>;
}

pub struct Dispatcher {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher {
    pub fn new() -> Self {
        let mut dispatcher = Self {
            tools: HashMap::new(),
        };

        for tool in [
            ("workspace.log", manager_roles()),
            ("workspace.note", manager_roles()),
            ("workspace.notes", manager_roles()),
            ("worker.create", manager_roles()),
            ("worker.assign", manager_roles()),
            ("worker.release", manager_roles()),
            ("worker.swap", manager_roles()),
            ("worker.grant", manager_roles()),
            ("worker.revoke", manager_roles()),
            ("job.create", manager_roles()),
            ("job.cancel", manager_roles()),
            ("job.keep_waiting", manager_roles()),
            ("job.update", manager_roles()),
            ("job.respond", manager_roles()),
            ("failover.confirm", manager_roles()),
            ("failover.cancel", manager_roles()),
            ("job.progress", worker_roles()),
            ("job.complete", worker_roles()),
            ("job.fail", worker_roles()),
            ("job.cancelled", worker_roles()),
            ("job.checkpoint", worker_roles()),
            ("job.request", worker_roles()),
            ("job.request_status", worker_roles()),
            ("file.read", worker_roles()),
            ("workspace.tree", worker_roles()),
            ("git.log", worker_roles()),
            ("git.diff", worker_roles()),
            ("context.ping", worker_roles()),
            ("context.checkpoint_defer", worker_roles()),
            ("subtask.create", worker_roles()),
            ("worker.notify", worker_roles()),
            ("workspace.status", shared_roles()),
            ("job.list_all", shared_roles()),
            ("job.status", shared_roles()),
            ("job.result", manager_roles()),
        ] {
            dispatcher.register(Box::new(StubTool::new(tool.0, tool.1)));
        }

        dispatcher
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn tools_for_role(&self, role: &WorkerRole) -> Vec<&str> {
        let mut tools = self
            .tools
            .values()
            .filter(|tool| tool.allowed_roles().contains(role))
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        tools.sort_unstable();
        tools
    }

    pub fn contains(&self, method: &str) -> bool {
        self.tools.contains_key(method)
    }

    pub async fn dispatch(
        &self,
        method: &str,
        params: Value,
        caller: &ConnectedClient,
    ) -> Result<Value, McpError> {
        let tool = self
            .tools
            .get(method)
            .ok_or_else(|| McpError::Internal(format!("unknown method: {method}")))?;

        if !tool.allowed_roles().contains(&caller.role) {
            return Err(McpError::Unauthorized {
                tool: method.to_string(),
                role: role_name(&caller.role).to_string(),
            });
        }

        tool.call(params, caller).await
    }
}

fn manager_roles() -> Vec<WorkerRole> {
    vec![WorkerRole::Manager]
}

fn worker_roles() -> Vec<WorkerRole> {
    vec![WorkerRole::Worker]
}

fn shared_roles() -> Vec<WorkerRole> {
    vec![WorkerRole::Manager, WorkerRole::Worker]
}

fn role_name(role: &WorkerRole) -> &'static str {
    match role {
        WorkerRole::Manager => "manager",
        WorkerRole::Worker => "worker",
    }
}

struct StubTool {
    name: &'static str,
    allowed_roles: Vec<WorkerRole>,
}

impl StubTool {
    fn new(name: &'static str, allowed_roles: Vec<WorkerRole>) -> Self {
        Self {
            name,
            allowed_roles,
        }
    }
}

#[async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        self.name
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &self.allowed_roles
    }

    async fn call(&self, _params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::Dispatcher;
    use crate::mcp::error::McpError;
    use crate::mcp::server::ConnectedClient;
    use crate::types::WorkerRole;
    use serde_json::Value;

    fn caller(role: WorkerRole) -> ConnectedClient {
        ConnectedClient {
            connection_id: "conn_1".to_string(),
            worker_id: Some("w1".to_string()),
            role,
            session_id: "sess_001".to_string(),
        }
    }

    #[tokio::test]
    async fn manager_tool_called_by_manager_returns_null() {
        let dispatcher = Dispatcher::new();
        let result = dispatcher
            .dispatch("worker.create", Value::Null, &caller(WorkerRole::Manager))
            .await
            .unwrap();
        assert_eq!(result, Value::Null);
    }

    #[tokio::test]
    async fn manager_tool_called_by_worker_returns_unauthorized() {
        let dispatcher = Dispatcher::new();
        let error = dispatcher
            .dispatch("worker.create", Value::Null, &caller(WorkerRole::Worker))
            .await
            .unwrap_err();

        assert!(matches!(error, McpError::Unauthorized { .. }));
    }

    #[test]
    fn tools_for_role_returns_distinct_tool_sets() {
        let dispatcher = Dispatcher::new();
        let manager_tools = dispatcher.tools_for_role(&WorkerRole::Manager);
        let worker_tools = dispatcher.tools_for_role(&WorkerRole::Worker);

        assert!(manager_tools.contains(&"worker.create"));
        assert!(!manager_tools.contains(&"job.progress"));
        assert!(worker_tools.contains(&"job.progress"));
        assert!(!worker_tools.contains(&"worker.create"));
        assert!(manager_tools.contains(&"workspace.status"));
        assert!(worker_tools.contains(&"workspace.status"));
    }
}
