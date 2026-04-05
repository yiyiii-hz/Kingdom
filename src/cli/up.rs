use std::path::{Path, PathBuf};

pub async fn run_up(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let workspace = workspace.canonicalize().unwrap_or_else(|_| workspace.clone());

    if !std::process::Command::new("which")
        .arg("tmux")
        .output()?
        .status
        .success()
    {
        return Err("tmux is required but not found".into());
    }

    let is_git = std::process::Command::new("git")
        .args(["-C", workspace.to_str().unwrap_or("."), "rev-parse", "--git-dir"])
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
    let pid_file = storage.root.join("daemon.pid");
    if pid_file.exists() {
        if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                let alive =
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok();
                if alive {
                    let cfg =
                        crate::config::KingdomConfig::load_or_default(&storage.root.join("config.toml"));
                    println!(
                        "Kingdom is already running. Use `tmux attach -t {}` to connect.",
                        cfg.tmux.session_name
                    );
                    return Ok(());
                }
            }
        }
    }

    let config = crate::config::KingdomConfig::load_or_default(&storage.root.join("config.toml"));
    let providers = crate::process::discovery::ProviderDiscovery::detect(&config);
    println!("\nAvailable providers:");
    for p in &providers {
        let key_status = if p.api_key_set {
            "API key set"
        } else {
            "no API key"
        };
        println!("  {} ({}) at {}", p.name, key_status, p.binary.display());
    }

    let available: Vec<_> = providers.iter().filter(|p| p.api_key_set).collect();
    if available.is_empty() {
        return Err("No providers available (check API key environment variables).".into());
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

    let session_name = &config.tmux.session_name;
    let has_session = std::process::Command::new("tmux")
        .args(["has-session", "-t", session_name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !has_session {
        std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", session_name])
            .status()?;
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

    println!("\nKingdom started. Provider: {manager_provider}");
    println!("  workspace hash: {hash}");
    println!("  tmux session: {session_name}");
    println!("  Attach with: tmux attach -t {session_name}");
    Ok(())
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
