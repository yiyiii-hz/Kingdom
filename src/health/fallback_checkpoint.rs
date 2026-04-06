use crate::types::CheckpointContent;
use chrono::Utc;
use std::path::Path;
use std::process::Command;

pub async fn generate_fallback_checkpoint(
    job_id: &str,
    workspace_path: &Path,
) -> CheckpointContent {
    let git_commit = git_current_commit(workspace_path);
    let id = format!("ckpt_fallback_{}", Utc::now().format("%Y%m%dT%H%M%S%3f"));
    let placeholder = "[自动生成，无摘要]".to_string();
    CheckpointContent {
        id,
        job_id: job_id.to_string(),
        created_at: Utc::now(),
        done: placeholder.clone(),
        abandoned: placeholder.clone(),
        in_progress: placeholder.clone(),
        remaining: placeholder.clone(),
        pitfalls: placeholder,
        git_commit,
    }
}

fn git_current_commit(workspace_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args([
            "-C",
            workspace_path.to_str().unwrap_or("."),
            "rev-parse",
            "HEAD",
        ])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fallback_checkpoint_has_placeholder_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let content = generate_fallback_checkpoint("job_001", tmp.path()).await;
        assert_eq!(content.job_id, "job_001");
        assert!(content.id.starts_with("ckpt_fallback_"));
        assert!(content.done.contains("自动生成"));
        assert!(content.remaining.contains("自动生成"));
    }
}
