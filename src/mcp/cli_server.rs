use serde_json::{json, Value};
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

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
}

impl CliServer {
    pub fn new(workspace_hash: &str) -> Self {
        Self {
            workspace_hash: workspace_hash.to_string(),
        }
    }

    pub async fn start(&self) -> Result<(), CliServerError> {
        tokio::fs::create_dir_all("/tmp/kingdom").await?;
        let path = self.socket_path();
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }

        let listener = UnixListener::bind(path)?;
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(value) => value,
                    Err(_) => break,
                };

                tokio::spawn(async move {
                    let (read_half, mut write_half) = tokio::io::split(stream);
                    let mut reader = BufReader::new(read_half);
                    let mut line = String::new();

                    if reader.read_line(&mut line).await.ok().filter(|bytes| *bytes > 0).is_none() {
                        return;
                    }

                    let response = match serde_json::from_str::<Value>(&line) {
                        Ok(request) => handle_command(&request),
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

fn handle_command(request: &Value) -> Value {
    match request.get("cmd").and_then(Value::as_str) {
        Some("ready") => json!({"ok": true, "data": {"status": "ready"}}),
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
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    #[tokio::test]
    async fn cli_server_returns_stub_responses() {
        let server = CliServer::new("m2-cli");
        server.start().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect("/tmp/kingdom/m2-cli-cli.sock")
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut bytes = serde_json::to_vec(&json!({"cmd":"ready"})).unwrap();
        bytes.push(b'\n');
        reader.get_mut().write_all(&bytes).await.unwrap();
        reader.get_mut().flush().await.unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["data"]["status"], "ready");
    }
}
