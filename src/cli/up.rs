use std::path::{Path, PathBuf};

pub async fn run_up(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());

    if !std::process::Command::new("which")
        .arg("tmux")
        .output()?
        .status
        .success()
    {
        return Err("tmux is required but not found".into());
    }

    let is_git = std::process::Command::new("git")
        .args([
            "-C",
            workspace.to_str().unwrap_or("."),
            "rev-parse",
            "--git-dir",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !is_git {
        print!("Warning: not a git repository. Continue without git? [Y/n] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim().to_lowercase() == "n" {
            return Err("Aborted.".into());
        }
    }

    let hash = crate::config::workspace_hash(&workspace);
    let storage = crate::storage::Storage::init(&workspace)?;
    let config = crate::config::KingdomConfig::load_or_default(&storage.root.join("config.toml"));
    let tmux = crate::tmux::TmuxController::new(config.tmux.session_name.clone());
    let pid_file = storage.root.join("daemon.pid");
    if check_and_clear_stale_pid(&pid_file, &config.tmux.session_name).is_some() {
        return Ok(());
    }

    let providers = crate::process::discovery::ProviderDiscovery::detect(&config);
    println!("\nAvailable providers:");
    for p in &providers {
        let auth_status = if p.api_key_set {
            "authenticated"
        } else {
            "installed (no env key — using CLI auth)"
        };
        println!("  {} ({}) at {}", p.name, auth_status, p.binary.display());
    }

    // Prefer providers with an env API key; fall back to all installed if none have one
    // (supports CLI-authenticated providers that don't need env vars).
    let keyed: Vec<_> = providers.iter().filter(|p| p.api_key_set).collect();
    let available: Vec<_> = if keyed.is_empty() {
        providers.iter().collect()
    } else {
        keyed
    };
    if available.is_empty() {
        return Err(
            "No AI providers found. Install claude, codex, or gemini and ensure they are on PATH."
                .into(),
        );
    }

    if !workspace.join("KINGDOM.md").exists() {
        print!("\nKINGDOM.md not found. Generate template? [Y/n] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim().to_lowercase() != "n" {
            let lang = detect_language(&workspace);
            let template = generate_kingdom_md(&lang);
            std::fs::write(workspace.join("KINGDOM.md"), template)?;
            println!("Created KINGDOM.md");
        }
    }

    let existing_session = storage.load_session()?;
    let resume_existing = existing_session
        .as_ref()
        .filter(|session| has_unfinished_jobs(session))
        .map(|session| confirm_resume(session))
        .transpose()?
        .unwrap_or(false);

    let manager_provider = if resume_existing {
        let mut session = existing_session.ok_or("resume_existing set but session is None")?;
        mark_old_manager_stale(&tmux, &mut session);
        storage.save_session(&session)?;
        session
            .manager_id
            .as_ref()
            .and_then(|id| session.workers.get(id))
            .map(|worker| worker.provider.clone())
            .unwrap_or_else(|| available[0].name.clone())
    } else {
        println!("\nChoose manager provider:");
        for (i, p) in available.iter().enumerate() {
            println!("  {}) {}", i + 1, p.name);
        }
        print!("Enter number [1]: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let idx = line.trim().parse::<usize>().unwrap_or(1).saturating_sub(1);
        let manager_provider = available.get(idx).unwrap_or(&available[0]).name.clone();

        let session = crate::types::Session {
            id: format!("sess_{}", uuid::Uuid::new_v4().simple()),
            workspace_path: workspace.display().to_string(),
            workspace_hash: hash.clone(),
            manager_id: Some("w0".to_string()),
            workers: [(
                "w0".to_string(),
                crate::types::Worker {
                    id: "w0".to_string(),
                    provider: manager_provider.clone(),
                    role: crate::types::WorkerRole::Manager,
                    status: crate::types::WorkerStatus::Starting,
                    job_id: None,
                    pid: None,
                    pane_id: String::new(),
                    mcp_connected: false,
                    context_usage_pct: None,
                    token_count: None,
                    last_heartbeat: None,
                    last_progress: None,
                    permissions: vec![],
                    started_at: chrono::Utc::now(),
                },
            )]
            .into_iter()
            .collect(),
            jobs: std::collections::HashMap::new(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: if is_git {
                crate::types::GitStrategy::Branch
            } else {
                crate::types::GitStrategy::None
            },
            available_providers: available
                .iter()
                .map(|provider| provider.name.clone())
                .collect(),
            notification_mode: if config.notifications.mode == "push" {
                crate::types::NotificationMode::Push
            } else {
                crate::types::NotificationMode::Poll
            },
            pending_requests: std::collections::HashMap::new(),
            pending_failovers: std::collections::HashMap::new(),
            provider_stability: std::collections::HashMap::new(),
            created_at: chrono::Utc::now(),
        };
        storage.save_session(&session)?;
        manager_provider
    };

    if !tmux.session_exists() {
        tmux.create_session(None)?;
    }

    let watchdog = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("kingdom-watchdog")))
        .unwrap_or_else(|| PathBuf::from("kingdom-watchdog"));

    std::process::Command::new(&watchdog)
        .arg(&workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if pid_file.exists() {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err("Daemon did not start within 10s.".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let manager_state = wait_for_manager_state(&storage, std::time::Duration::from_secs(20)).await;
    let session_name = &config.tmux.session_name;

    println!("\nKingdom started. Provider: {manager_provider}");
    println!("  workspace hash: {hash}");
    println!("  tmux session: {session_name}");
    match manager_state {
        ManagerStartupState::Connected { pid, pane_id } => {
            println!("  manager pid: {pid}");
            println!("  manager pane: {pane_id}");
        }
        ManagerStartupState::Failed { reason } => {
            println!("  manager startup degraded: {reason}");
        }
        ManagerStartupState::Pending => {
            println!("  manager startup pending: waiting for MCP connection");
        }
    }
    println!("  Attach with: tmux attach -t {session_name}");
    Ok(())
}

fn check_and_clear_stale_pid(pid_file: &Path, session_name: &str) -> Option<()> {
    if pid_file.exists() {
        if let Ok(pid_str) = std::fs::read_to_string(pid_file) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                let alive =
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok();
                if alive {
                    println!(
                        "Kingdom is already running. Use `tmux attach -t {}` to connect.",
                        session_name
                    );
                    return Some(());
                }
            }
        }
        println!("发现残留 daemon.pid（进程已退出），正在清理...");
        let _ = std::fs::remove_file(pid_file);
    }
    None
}

fn has_unfinished_jobs(session: &crate::types::Session) -> bool {
    session.jobs.values().any(|job| {
        !matches!(
            job.status,
            crate::types::JobStatus::Completed
                | crate::types::JobStatus::Cancelled
                | crate::types::JobStatus::Failed
        )
    })
}

fn confirm_resume(session: &crate::types::Session) -> Result<bool, Box<dyn std::error::Error>> {
    use std::io::Write;

    println!("\n检测到上次 session 有未完成工作：\n");
    let mut jobs = session.jobs.values().collect::<Vec<_>>();
    jobs.sort_by_key(|job| job.id.clone());
    for job in jobs {
        println!("  {:<8} {:<24} {}", job.id, truncate(&job.intent, 20), describe_job(session, job));
    }
    if !session.notes.is_empty() {
        println!("\nworkspace.notes:");
        for note in session.notes.iter().take(5) {
            println!("  · {}", truncate(&note.content, 100));
        }
    }
    print!("\n继续上次工作？[Y/n] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

fn describe_job(session: &crate::types::Session, job: &crate::types::Job) -> String {
    match job.status {
        crate::types::JobStatus::Running => {
            let detail = job
                .worker_id
                .as_ref()
                .and_then(|worker_id| session.workers.get(worker_id))
                .map(|worker| {
                    if worker.mcp_connected {
                        format!("{worker_id} 进行中", worker_id = worker.id)
                    } else {
                        format!("{worker_id} 已暂停", worker_id = worker.id)
                    }
                })
                .unwrap_or_else(|| "未绑定 worker".to_string());
            format!("[running → {detail}]")
        }
        crate::types::JobStatus::Waiting => {
            if job.depends_on.is_empty() {
                "[waiting]".to_string()
            } else {
                format!("[waiting → 依赖 {}]", job.depends_on.join(", "))
            }
        }
        crate::types::JobStatus::Completed => "[completed ✓]".to_string(),
        crate::types::JobStatus::Failed => "[failed]".to_string(),
        crate::types::JobStatus::Pending => "[pending]".to_string(),
        crate::types::JobStatus::Paused => "[paused]".to_string(),
        crate::types::JobStatus::Cancelled => "[cancelled]".to_string(),
        crate::types::JobStatus::Cancelling => "[cancelling]".to_string(),
    }
}

fn truncate(input: &str, max_chars: usize) -> String {
    let count = input.chars().count();
    if count <= max_chars {
        return input.to_string();
    }
    input.chars().take(max_chars).collect::<String>() + "…"
}

fn mark_old_manager_stale(tmux: &crate::tmux::TmuxController, session: &mut crate::types::Session) {
    let Some(manager_id) = session.manager_id.clone() else {
        return;
    };
    let Some(manager) = session.workers.get_mut(&manager_id) else {
        return;
    };
    if !manager.pane_id.is_empty() {
        let _ = tmux.inject_line(
            &manager.pane_id,
            "[Kingdom] 此 manager 已被新 manager 接手，请切换到新 manager pane",
        );
    }
    manager.status = crate::types::WorkerStatus::Failed;
    manager.mcp_connected = false;
    manager.pid = None;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ManagerStartupState {
    Connected { pid: u32, pane_id: String },
    Failed { reason: String },
    Pending,
}

async fn wait_for_manager_state(
    storage: &crate::storage::Storage,
    timeout: std::time::Duration,
) -> ManagerStartupState {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let Some(session) = storage.load_session().ok().flatten() else {
            return ManagerStartupState::Pending;
        };
        let Some(manager_id) = session.manager_id.as_ref() else {
            return ManagerStartupState::Pending;
        };
        let Some(manager) = session.workers.get(manager_id) else {
            return ManagerStartupState::Pending;
        };
        if manager.mcp_connected {
            if let (Some(pid), false) = (manager.pid, manager.pane_id.is_empty()) {
                return ManagerStartupState::Connected {
                    pid,
                    pane_id: manager.pane_id.clone(),
                };
            }
        }
        if manager.status == crate::types::WorkerStatus::Failed {
            let reason = storage
                .read_action_log(Some(10))
                .ok()
                .and_then(|entries| {
                    entries
                        .into_iter()
                        .rev()
                        .find(|entry| entry.action == "manager.start_failed")
                        .and_then(|entry| entry.error)
                })
                .unwrap_or_else(|| "manager start failed".to_string());
            return ManagerStartupState::Failed { reason };
        }
        if std::time::Instant::now() > deadline {
            return ManagerStartupState::Pending;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn detect_language(workspace: &Path) -> String {
    if workspace.join("Cargo.toml").exists() {
        return "Rust".to_string();
    }
    if workspace.join("package.json").exists() {
        return "TypeScript/JavaScript".to_string();
    }
    if workspace.join("pyproject.toml").exists() {
        return "Python".to_string();
    }
    if workspace.join("go.mod").exists() {
        return "Go".to_string();
    }
    "（未检测到）".to_string()
}

fn generate_kingdom_md(lang: &str) -> String {
    format!(
        r#"# Kingdom 工作约束

## 代码规范
- 语言：{lang}
- 禁止：（在此描述不允许的写法，如 unwrap()、any、print debugging）

## 架构约束
- （在此描述不能改动的架构决策）

## 风格偏好
- （在此描述 AI 应遵守的代码风格）
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_language_prefers_rust() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_language(tmp.path()), "Rust");
    }

    #[test]
    fn detect_language_falls_back_to_javascript_and_unknown() {
        let js = tempfile::tempdir().unwrap();
        std::fs::write(js.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_language(js.path()), "TypeScript/JavaScript");

        let unknown = tempfile::tempdir().unwrap();
        assert_eq!(detect_language(unknown.path()), "（未检测到）");
    }

    #[test]
    fn generate_kingdom_md_contains_expected_sections() {
        let doc = generate_kingdom_md("Rust");
        assert!(doc.contains("# Kingdom 工作约束"));
        assert!(doc.contains("## 代码规范"));
        assert!(doc.contains("语言：Rust"));
        assert!(doc.contains("## 架构约束"));
        assert!(doc.contains("## 风格偏好"));
    }

    #[test]
    fn test_up_clears_stale_pid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("daemon.pid");
        std::fs::write(&pid_file, "99999999\n").unwrap();

        let result = check_and_clear_stale_pid(&pid_file, "kingdom");

        assert!(result.is_none(), "stale pid must not be treated as running");
        assert!(!pid_file.exists(), "stale daemon.pid should be removed");
    }

    #[tokio::test]
    async fn wait_for_manager_state_reads_connected_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = crate::storage::Storage::init(tmp.path()).unwrap();
        let mut session = crate::types::Session {
            id: "sess_1".to_string(),
            workspace_path: tmp.path().display().to_string(),
            workspace_hash: "hash".to_string(),
            manager_id: Some("w0".to_string()),
            workers: [(
                "w0".to_string(),
                crate::types::Worker {
                    id: "w0".to_string(),
                    provider: "codex".to_string(),
                    role: crate::types::WorkerRole::Manager,
                    status: crate::types::WorkerStatus::Idle,
                    job_id: None,
                    pid: Some(42),
                    pane_id: "%1".to_string(),
                    mcp_connected: true,
                    context_usage_pct: None,
                    token_count: None,
                    last_heartbeat: None,
                    last_progress: None,
                    permissions: vec![],
                    started_at: chrono::Utc::now(),
                },
            )]
            .into_iter()
            .collect(),
            jobs: std::collections::HashMap::new(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: crate::types::GitStrategy::None,
            available_providers: vec!["codex".to_string()],
            notification_mode: crate::types::NotificationMode::Poll,
            pending_requests: std::collections::HashMap::new(),
            pending_failovers: std::collections::HashMap::new(),
            provider_stability: std::collections::HashMap::new(),
            created_at: chrono::Utc::now(),
        };
        storage.save_session(&session).unwrap();
        assert_eq!(
            wait_for_manager_state(&storage, std::time::Duration::from_millis(10)).await,
            ManagerStartupState::Connected {
                pid: 42,
                pane_id: "%1".to_string(),
            }
        );

        session.workers.get_mut("w0").unwrap().mcp_connected = false;
        session.workers.get_mut("w0").unwrap().status = crate::types::WorkerStatus::Failed;
        storage.save_session(&session).unwrap();
        storage
            .append_action_log(&crate::types::ActionLogEntry {
                timestamp: chrono::Utc::now(),
                actor: "kingdom-daemon".to_string(),
                action: "manager.start_failed".to_string(),
                params: serde_json::json!({ "worker_id": "w0" }),
                result: None,
                error: Some("connect timeout".to_string()),
            })
            .unwrap();
        assert_eq!(
            wait_for_manager_state(&storage, std::time::Duration::from_millis(10)).await,
            ManagerStartupState::Failed {
                reason: "connect timeout".to_string(),
            }
        );
    }
}
