use serde_json::Value;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub fn socket_path(workspace: &Path) -> String {
    let hash = crate::config::workspace_hash(workspace);
    format!("/tmp/kingdom/{hash}-cli.sock")
}

pub async fn send_cli_command(
    socket_path: &str,
    request: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|_| "Kingdom daemon 未运行")?;
    let mut reader = BufReader::new(stream);
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    reader.get_mut().write_all(&bytes).await?;
    reader.get_mut().flush().await?;

    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let response: Value = serde_json::from_str(&line)?;
    if response["ok"].as_bool() != Some(true) {
        let error = response["error"]
            .as_str()
            .unwrap_or("unknown error")
            .to_string();
        return Err(error.into());
    }
    Ok(response)
}
