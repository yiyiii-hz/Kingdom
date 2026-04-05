use std::path::{Path, PathBuf};

pub async fn run_down(
    workspace: PathBuf,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let workspace = workspace.canonicalize().unwrap_or_else(|_| workspace.clone());
    let storage = crate::storage::Storage::init(&workspace)?;
    let storage_root = storage.root.clone();
    let session = storage
        .load_session()?
        .ok_or("No active Kingdom session found.")?;

    let running_jobs: Vec<_> = session
        .jobs
        .values()
        .filter(|j| j.status == crate::types::JobStatus::Running)
        .map(|j| j.id.clone())
        .collect();

    if !force && !running_jobs.is_empty() {
        println!("{} job(s) still running.", running_jobs.len());
        println!("  1) Wait for completion");
        println!("  2) Suspend and exit");
        println!("  3) Force quit now");
        print!("Choice [3]: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        match line.trim() {
            "1" => loop {
                let s = storage.load_session()?.ok_or("Session gone")?;
                let still_running = s
                    .jobs
                    .values()
                    .any(|j| j.status == crate::types::JobStatus::Running);
                if !still_running {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            },
            "2" => {
                println!("Suspending... (checkpoint not yet implemented, exiting anyway)");
            }
            _ => {}
        }
    }

    terminate_by_pid_file(&storage_root.join("daemon.pid"), true)?;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    terminate_by_pid_file(&storage_root.join("watchdog.pid"), false)?;

    let hash = crate::config::workspace_hash(&workspace);
    let _ = std::fs::remove_file(format!("/tmp/kingdom/{hash}.sock"));
    let _ = std::fs::remove_file(format!("/tmp/kingdom/{hash}-cli.sock"));

    println!("Kingdom stopped.");
    Ok(())
}

fn terminate_by_pid_file(
    pid_file: &Path,
    graceful: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !pid_file.exists() {
        return Ok(());
    }
    let pid_str = std::fs::read_to_string(pid_file)?;
    if let Ok(pid) = pid_str.trim().parse::<u32>() {
        let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
        if graceful {
            let _ = nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM);
        } else {
            let _ = nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGKILL);
        }
    }
    let _ = std::fs::remove_file(pid_file);
    Ok(())
}
