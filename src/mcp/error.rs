use serde_json::{json, Value};
use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub enum McpError {
    Unauthorized { tool: String, role: String },
    JobNotFound(String),
    WorkerNotFound(String),
    InvalidState { message: String },
    ValidationFailed { field: String, reason: String },
    Internal(String),
}

impl McpError {
    pub fn to_jsonrpc_error(&self) -> Value {
        let (code, message) = match self {
            Self::Unauthorized { tool, role } => (
                -32001,
                format!("role {role} is not authorized to call {tool}"),
            ),
            Self::JobNotFound(job_id) => (-32002, format!("job not found: {job_id}")),
            Self::WorkerNotFound(worker_id) => (-32002, format!("worker not found: {worker_id}")),
            Self::InvalidState { message } => (-32003, message.clone()),
            Self::ValidationFailed { field, reason } => {
                (-32004, format!("validation failed for {field}: {reason}"))
            }
            Self::Internal(message) => (-32603, message.clone()),
        };

        json!({
            "code": code,
            "message": message,
        })
    }
}

impl Display for McpError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized { tool, role } => {
                write!(f, "role {role} is not authorized to call {tool}")
            }
            Self::JobNotFound(job_id) => write!(f, "job not found: {job_id}"),
            Self::WorkerNotFound(worker_id) => write!(f, "worker not found: {worker_id}"),
            Self::InvalidState { message } => write!(f, "{message}"),
            Self::ValidationFailed { field, reason } => {
                write!(f, "validation failed for {field}: {reason}")
            }
            Self::Internal(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for McpError {}

#[cfg(test)]
mod tests {
    use super::McpError;

    #[test]
    fn to_jsonrpc_error_uses_expected_codes() {
        let cases = [
            (
                McpError::Unauthorized {
                    tool: "worker.create".to_string(),
                    role: "worker".to_string(),
                },
                -32001,
            ),
            (McpError::JobNotFound("job_001".to_string()), -32002),
            (McpError::WorkerNotFound("w1".to_string()), -32002),
            (
                McpError::InvalidState {
                    message: "bad state".to_string(),
                },
                -32003,
            ),
            (
                McpError::ValidationFailed {
                    field: "session_id".to_string(),
                    reason: "mismatch".to_string(),
                },
                -32004,
            ),
            (McpError::Internal("boom".to_string()), -32603),
        ];

        for (error, code) in cases {
            assert_eq!(error.to_jsonrpc_error()["code"], code);
        }
    }
}
