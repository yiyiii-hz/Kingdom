use crate::config::KingdomConfig;
use crate::process::adapter::adapter_for;
use crate::types::WorkerRole;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct LaunchResult {
    pub pid: u32,
    pub pane_id: String,
}

#[derive(Debug)]
pub enum LaunchError {
    TmuxNotFound,
    TmuxFailed(String),
    Io(std::io::Error),
    Other(String),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TmuxNotFound => write!(f, "tmux not found"),
            Self::TmuxFailed(s) => write!(f, "tmux failed: {s}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for LaunchError {}

pub struct ProcessLauncher {
    pub workspace_path: PathBuf,
    pub config: KingdomConfig,
    pub workspace_hash: String,
}

impl ProcessLauncher {
    pub fn new(workspace_path: PathBuf, config: KingdomConfig, workspace_hash: String) -> Self {
        Self {
            workspace_path,
            config,
            workspace_hash,
        }
    }

    pub async fn launch(
        &self,
        provider: &str,
        role: WorkerRole,
        worker_id: &str,
        worker_index: usize,
        storage_root: &Path,
    ) -> Result<LaunchResult, LaunchError> {
        let mcp_dir = storage_root.join("mcp");
        tokio::fs::create_dir_all(&mcp_dir)
            .await
            .map_err(LaunchError::Io)?;
        let mcp_config_path = mcp_dir.join(format!("{worker_id}.json"));
        let mcp_config = self.build_mcp_config(worker_id, role.clone());
        tokio::fs::write(&mcp_config_path, mcp_config)
            .await
            .map_err(LaunchError::Io)?;

        let binary = crate::process::discovery::ProviderDiscovery::check(provider, &self.config)
            .ok_or_else(|| LaunchError::Other(format!("provider '{provider}' not found")))?;

        let session_name = &self.config.tmux.session_name;
        let pane_id = self.create_pane(session_name, worker_id, worker_index)?;

        let adapter = adapter_for(provider, binary);
        let argv = adapter.build_args(&mcp_config_path, role);
        let cmd = argv.join(" ");
        self.tmux_send_keys(&pane_id, &cmd)?;

        let pid = self.get_pane_pid(&pane_id)?;
        Ok(LaunchResult { pid, pane_id })
    }

    pub async fn terminate(&self, pid: u32, graceful: bool) -> Result<(), LaunchError> {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;

        let nix_pid = Pid::from_raw(pid as i32);
        if !graceful {
            let _ = kill(nix_pid, Signal::SIGKILL);
            return Ok(());
        }

        let _ = kill(nix_pid, Signal::SIGTERM);
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let _ = kill(nix_pid, Signal::SIGKILL);
        Ok(())
    }

    fn create_pane(
        &self,
        session_name: &str,
        worker_id: &str,
        worker_index: usize,
    ) -> Result<String, LaunchError> {
        let output = if worker_index == 0 {
            std::process::Command::new("tmux")
                .args([
                    "split-window",
                    "-h",
                    "-t",
                    &format!("{session_name}:0"),
                    "-P",
                    "-F",
                    "#{pane_id}",
                ])
                .output()
        } else if worker_index <= 2 {
            std::process::Command::new("tmux")
                .args([
                    "split-window",
                    "-v",
                    "-t",
                    &format!("{session_name}:0"),
                    "-P",
                    "-F",
                    "#{pane_id}",
                ])
                .output()
        } else {
            std::process::Command::new("tmux")
                .args([
                    "new-window",
                    "-n",
                    &format!("kingdom:{worker_id}"),
                    "-P",
                    "-F",
                    "#{pane_id}",
                ])
                .output()
        }
        .map_err(|_| LaunchError::TmuxNotFound)?;

        if !output.status.success() {
            return Err(LaunchError::TmuxFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn tmux_send_keys(&self, pane_id: &str, cmd: &str) -> Result<(), LaunchError> {
        let status = std::process::Command::new("tmux")
            .args(["send-keys", "-t", pane_id, cmd, "Enter"])
            .status()
            .map_err(|_| LaunchError::TmuxNotFound)?;
        if !status.success() {
            return Err(LaunchError::TmuxFailed("send-keys failed".into()));
        }
        Ok(())
    }

    fn get_pane_pid(&self, pane_id: &str) -> Result<u32, LaunchError> {
        let output = std::process::Command::new("tmux")
            .args(["display-message", "-t", pane_id, "-p", "#{pane_pid}"])
            .output()
            .map_err(|_| LaunchError::TmuxNotFound)?;
        let pid_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        pid_str
            .parse::<u32>()
            .map_err(|_| LaunchError::Other(format!("invalid pid: {pid_str}")))
    }

    fn build_mcp_config(&self, worker_id: &str, role: WorkerRole) -> String {
        let socket = format!("/tmp/kingdom/{}.sock", self.workspace_hash);
        let role_str = match role {
            WorkerRole::Manager => "manager",
            WorkerRole::Worker => "worker",
        };
        serde_json::json!({
            "mcpServers": {
                "kingdom": {
                    "command": "socat",
                    "args": [format!("UNIX-CONNECT:{socket}"), "-"],
                    "env": {
                        "KINGDOM_WORKER_ID": worker_id,
                        "KINGDOM_ROLE": role_str
                    }
                }
            }
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KingdomConfig;
    use crate::test_support::{env_lock, PathGuard};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn write_executable(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn with_fake_tmux<F>(test: F)
    where
        F: FnOnce(&Path),
    {
        let _env_lock = env_lock();
        let tmp = tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let log_path = tmp.path().join("tmux.log");
        let tmux_path = bin_dir.join("tmux");
        write_executable(
            &tmux_path,
            &format!(
                "#!/bin/sh\n\
                echo \"$@\" >> \"{}\"\n\
                case \"$1\" in\n\
                  split-window) echo %1 ;;\n\
                  new-window) echo %4 ;;\n\
                  display-message) echo 4242 ;;\n\
                  send-keys) exit 0 ;;\n\
                  *) exit 0 ;;\n\
                esac\n",
                log_path.display()
            ),
        );
        let _path_guard = PathGuard::prepend(&bin_dir);
        test(tmp.path());
    }

    #[test]
    fn launch_returns_pid_and_pane_id() {
        with_fake_tmux(|tmp| {
            let workspace = tmp.join("workspace");
            fs::create_dir_all(&workspace).unwrap();
            let storage_root = tmp.join("storage");
            fs::create_dir_all(&storage_root).unwrap();
            let provider = tmp.join("codex");
            write_executable(&provider, "#!/bin/sh\nexit 0\n");

            let mut config = KingdomConfig::default_config();
            config
                .providers
                .overrides
                .insert("codex".to_string(), provider.display().to_string());
            let launcher = ProcessLauncher::new(workspace, config, "hash123".to_string());

            let rt = tokio::runtime::Runtime::new().unwrap();
            let result = rt.block_on(launcher.launch(
                "codex",
                WorkerRole::Worker,
                "w1",
                1,
                &storage_root,
            ));
            let result = result.unwrap();

            assert_eq!(result.pid, 4242);
            assert_eq!(result.pane_id, "%1");
            let mcp = fs::read_to_string(storage_root.join("mcp/w1.json")).unwrap();
            assert!(mcp.contains("KINGDOM_WORKER_ID"));
            assert!(mcp.contains("w1"));
        });
    }

    #[test]
    fn fourth_worker_launch_uses_new_tmux_window() {
        with_fake_tmux(|tmp| {
            let workspace = tmp.join("workspace");
            fs::create_dir_all(&workspace).unwrap();
            let storage_root = tmp.join("storage");
            fs::create_dir_all(&storage_root).unwrap();
            let provider = tmp.join("codex");
            write_executable(&provider, "#!/bin/sh\nexit 0\n");

            let mut config = KingdomConfig::default_config();
            config
                .providers
                .overrides
                .insert("codex".to_string(), provider.display().to_string());
            let launcher = ProcessLauncher::new(workspace, config, "hash123".to_string());

            let rt = tokio::runtime::Runtime::new().unwrap();
            let result = rt.block_on(launcher.launch(
                "codex",
                WorkerRole::Worker,
                "w4",
                3,
                &storage_root,
            ));
            let result = result.unwrap();

            assert_eq!(result.pane_id, "%4");
            let log = fs::read_to_string(tmp.join("tmux.log")).unwrap();
            assert!(log.contains("new-window"));
            assert!(log.contains("kingdom:w4"));
        });
    }
}
