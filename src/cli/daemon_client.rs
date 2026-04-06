use serde_json::Value;
use std::path::Path;
use std::time::Duration;
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
    let stream = connect_with_retry(socket_path, Duration::from_secs(3))
        .await
        .map_err(|_| "Kingdom daemon 未运行（socket 不存在或无响应）")?;
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

async fn connect_with_retry(
    socket_path: &str,
    timeout: Duration,
) -> Result<UnixStream, std::io::Error> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut delay_ms = 200u64;
    loop {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(err);
                }
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(800);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_connect_with_retry_fails_fast_on_missing_socket() {
        let socket_path = format!(
            "/tmp/kingdom-missing-{}.sock",
            uuid::Uuid::new_v4().simple()
        );
        let start = std::time::Instant::now();

        let result = connect_with_retry(&socket_path, Duration::from_millis(300)).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "missing socket should return an error");
        assert!(
            elapsed >= Duration::from_millis(200),
            "expected at least one retry, elapsed: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "retry loop should stop quickly for short timeout, elapsed: {elapsed:?}"
        );
    }
}
