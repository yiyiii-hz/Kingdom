use crate::storage::Storage;
use crate::types::WorkerRole;
use std::path::{Path, PathBuf};

pub async fn run_restart(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;
    let daemon_pid_path = storage.root.join("daemon.pid");
    let watchdog_pid_path = storage.root.join("watchdog.pid");

    let pid = match read_live_pid(&daemon_pid_path)? {
        Some(pid) => pid,
        None => {
            if daemon_pid_path.exists() {
                println!("daemon 进程已退出（残留 pid 文件），正在清理...");
                let _ = std::fs::remove_file(&daemon_pid_path);
            } else {
                println!("Kingdom daemon 未运行");
                return Ok(());
            }
            0
        }
    };

    if pid != 0 {
        println!("正在停止 daemon (PID {pid})...");
        stop_process(pid).await?;
    }

    if let Some(watchdog_pid) = read_live_pid(&watchdog_pid_path)? {
        println!("等待 watchdog 重启 daemon...");
        let _ = watchdog_pid;
    } else {
        let watchdog_binary = watchdog_binary_path();
        std::process::Command::new(&watchdog_binary)
            .arg(&workspace)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
    }

    let new_pid = wait_for_new_daemon_pid(&daemon_pid_path, pid).await?;
    println!("✓ daemon 已重启，PID {new_pid}");
    print_provider_status(&storage)?;
    Ok(())
}

fn read_live_pid(pid_file: &Path) -> Result<Option<u32>, Box<dyn std::error::Error>> {
    if !pid_file.exists() {
        return Ok(None);
    }
    let pid = std::fs::read_to_string(pid_file)?
        .trim()
        .parse::<u32>()
        .ok();
    Ok(pid.filter(|pid| is_pid_alive(*pid)))
}

async fn stop_process(pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    let _ = nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() <= deadline {
        if !is_pid_alive(pid) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    println!("daemon 未响应，强制终止");
    let _ = nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGKILL);
    Ok(())
}

async fn wait_for_new_daemon_pid(
    daemon_pid_path: &Path,
    old_pid: u32,
) -> Result<u32, Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while tokio::time::Instant::now() <= deadline {
        if let Some(pid) = read_live_pid(daemon_pid_path)? {
            if pid != old_pid {
                return Ok(pid);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err("⚠ daemon 未能在 15 秒内重启，请检查日志".into())
}

fn print_provider_status(storage: &Storage) -> Result<(), Box<dyn std::error::Error>> {
    let Some(session) = storage.load_session()? else {
        return Ok(());
    };
    println!();
    println!("Provider 进程状态：");
    let mut workers = session.workers.values().collect::<Vec<_>>();
    workers.sort_by_key(|worker| worker.id.clone());
    for worker in workers {
        if worker.role == WorkerRole::Manager {
            continue;
        }
        match worker.pid {
            Some(pid) if is_pid_alive(pid) => {
                println!("  ✓ {} ({})  PID {}  进程存活", worker.id, worker.provider, pid);
            }
            Some(pid) => {
                println!(
                    "  ⚠ {} ({})  PID {}  进程已退出（需手动处理）",
                    worker.id, worker.provider, pid
                );
            }
            None => {
                println!(
                    "  ⚠ {} ({})  PID -  进程已退出（需手动处理）",
                    worker.id, worker.provider
                );
            }
        }
    }
    Ok(())
}

fn watchdog_binary_path() -> PathBuf {
    let current = std::env::current_exe().ok();
    current
        .as_ref()
        .and_then(|path| path.parent().map(|dir| dir.join("kingdom-watchdog")))
        .filter(|path| path.exists())
        .or_else(|| {
            current.as_ref().and_then(|path| {
                path.parent()
                    .and_then(|dir| dir.parent().map(|parent| parent.join("kingdom-watchdog")))
            })
        })
        .unwrap_or_else(|| PathBuf::from("kingdom-watchdog"))
}

fn is_pid_alive(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_restart_missing_pid_file() {
        let temp = tempfile::tempdir().unwrap();
        assert!(run_restart(temp.path().to_path_buf()).await.is_ok());
    }

    #[test]
    fn test_restart_stale_pid_file() {
        let temp = tempfile::tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let pid_file = storage.root.join("daemon.pid");
        std::fs::write(&pid_file, "99999999\n").unwrap();

        let pid = read_live_pid(&pid_file).unwrap();
        assert!(pid.is_none());
        let _ = std::fs::remove_file(&pid_file);
        assert!(!pid_file.exists());
    }
}
