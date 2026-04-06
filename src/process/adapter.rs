use crate::types::WorkerRole;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub trait ProviderAdapter: Send + Sync {
    fn build_args(&self, mcp_config_path: &Path, role: WorkerRole) -> Vec<String>;
    fn working_dir(&self, workspace_path: &Path) -> Option<PathBuf>;
    fn is_clean_exit(&self, code: i32) -> bool {
        code == 0
    }
    fn connection_grace_period(&self) -> Duration;
}

pub struct ClaudeAdapter {
    pub binary: PathBuf,
}

pub struct CodexAdapter {
    pub binary: PathBuf,
}

pub struct GeminiAdapter {
    pub binary: PathBuf,
}

impl ProviderAdapter for ClaudeAdapter {
    fn build_args(&self, mcp_config_path: &Path, _role: WorkerRole) -> Vec<String> {
        vec![
            self.binary.to_string_lossy().into_owned(),
            "--dangerously-skip-permissions".into(),
            "--mcp-config".into(),
            mcp_config_path.to_string_lossy().into_owned(),
        ]
    }

    fn working_dir(&self, _workspace_path: &Path) -> Option<PathBuf> {
        None
    }

    fn connection_grace_period(&self) -> Duration {
        Duration::from_secs(3)
    }
}

impl ProviderAdapter for CodexAdapter {
    fn build_args(&self, mcp_config_path: &Path, _role: WorkerRole) -> Vec<String> {
        vec![
            self.binary.to_string_lossy().into_owned(),
            "--mcp-config".into(),
            mcp_config_path.to_string_lossy().into_owned(),
        ]
    }

    fn working_dir(&self, _workspace_path: &Path) -> Option<PathBuf> {
        None
    }

    fn connection_grace_period(&self) -> Duration {
        Duration::from_secs(5)
    }
}

impl ProviderAdapter for GeminiAdapter {
    fn build_args(&self, mcp_config_path: &Path, _role: WorkerRole) -> Vec<String> {
        vec![
            self.binary.to_string_lossy().into_owned(),
            "--mcp-config".into(),
            mcp_config_path.to_string_lossy().into_owned(),
        ]
    }

    fn working_dir(&self, _workspace_path: &Path) -> Option<PathBuf> {
        None
    }

    fn connection_grace_period(&self) -> Duration {
        Duration::from_secs(5)
    }
}

pub fn adapter_for(provider: &str, binary: PathBuf) -> Box<dyn ProviderAdapter> {
    match provider {
        "claude" => Box::new(ClaudeAdapter { binary }),
        "codex" => Box::new(CodexAdapter { binary }),
        "gemini" => Box::new(GeminiAdapter { binary }),
        _ => Box::new(CodexAdapter { binary }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::WorkerRole;

    #[test]
    fn claude_adapter_args() {
        let adapter = ClaudeAdapter {
            binary: PathBuf::from("/usr/local/bin/claude"),
        };
        let args = adapter.build_args(Path::new("/tmp/kingdom/mcp/w1.json"), WorkerRole::Worker);
        assert_eq!(args[0], "/usr/local/bin/claude");
        assert!(args.contains(&"--mcp-config".to_string()));
        assert!(args.contains(&"/tmp/kingdom/mcp/w1.json".to_string()));
    }

    #[test]
    fn adapter_for_returns_correct_grace_period() {
        let claude = adapter_for("claude", PathBuf::from("claude"));
        assert_eq!(claude.connection_grace_period(), Duration::from_secs(3));
        let codex = adapter_for("codex", PathBuf::from("codex"));
        assert_eq!(codex.connection_grace_period(), Duration::from_secs(5));
    }
}
