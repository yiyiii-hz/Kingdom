use crate::config::{workspace_hash, HealthConfig, KingdomConfig};
use crate::storage::Storage;
use crate::types::{Session, Worker, WorkerRole};
use chrono::{DateTime, Utc};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run_doctor(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;
    print!("{}", render_doctor_report(&workspace, &storage));
    Ok(())
}

fn render_doctor_report(workspace: &Path, storage: &Storage) -> String {
    let mut output = String::from("检查 Kingdom 运行环境...\n\n");

    output.push_str("[系统依赖]\n");
    for line in dependency_lines() {
        let _ = writeln!(output, "{line}");
    }

    output.push_str("\n[API Key]\n");
    for line in api_key_lines() {
        let _ = writeln!(output, "{line}");
    }

    let daemon = daemon_status(workspace, &storage.root);
    output.push_str("\n[Kingdom Daemon]\n");
    match &daemon {
        Some(info) => {
            let elapsed = Utc::now()
                .signed_duration_since(info.started_at)
                .to_std()
                .unwrap_or_default()
                .as_secs();
            let _ = writeln!(
                output,
                "✓ daemon 运行中  PID {}  已运行 {}",
                info.pid,
                format_duration(elapsed)
            );
            if info.socket_exists {
                let _ = writeln!(output, "✓ MCP socket    {}", info.socket_path.display());
            } else {
                let _ = writeln!(
                    output,
                    "⚠ MCP socket    {}  → kingdom restart",
                    info.socket_path.display()
                );
            }
            if let Some(pid) = info.watchdog_pid {
                let _ = writeln!(output, "✓ watchdog      运行中  PID {pid}");
            } else {
                output.push_str("⚠ watchdog      未运行\n");
            }
        }
        None => {
            output.push_str("[Kingdom Daemon] 未运行，跳过\n");
        }
    }

    if daemon.is_some() {
        output.push_str("\n[当前 Session]\n");
        match storage.load_session() {
            Ok(Some(session)) => {
                let config = KingdomConfig::load_or_default(&storage.root.join("config.toml"));
                for line in session_lines(&session, &config.health, Utc::now()) {
                    let _ = writeln!(output, "{line}");
                }
            }
            _ => output.push_str("⚠ state.json 不存在\n"),
        }
    } else {
        output.push_str("\n[当前 Session] 未运行，跳过\n");
    }

    output.push_str("\n[配置文件]\n");
    let config_path = storage.root.join("config.toml");
    if config_path.exists() {
        match std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|text| toml::from_str::<KingdomConfig>(&text).ok())
        {
            Some(_) => {
                let _ = writeln!(output, "✓ .kingdom/config.toml    有效");
            }
            None => {
                let _ = writeln!(output, "✗ .kingdom/config.toml    解析失败");
            }
        }
    } else {
        let _ = writeln!(output, "⚠ .kingdom/config.toml    不存在");
    }

    let kingdom_md = workspace.join("KINGDOM.md");
    let _ = writeln!(
        output,
        "{} KINGDOM.md              {}",
        if kingdom_md.exists() { "✓" } else { "✗" },
        if kingdom_md.exists() { "存在" } else { "缺失" }
    );

    if let Some(info) = daemon {
        if !info.socket_exists {
            output.push_str("⚠ socket 丢失 → kingdom restart\n");
        }
    }

    output
}

fn dependency_lines() -> Vec<String> {
    vec![
        versioned_binary_line("tmux", &["-V"], "→ brew install tmux"),
        versioned_binary_line("git", &["--version"], "→ brew install git"),
        binary_line("codex", "→ npm install -g @openai/codex"),
        binary_line(
            "claude",
            "→ npm install -g @anthropic-ai/claude-code 或 pip install claude-code",
        ),
        binary_line(
            "gemini",
            "→ 参考 https://ai.google.dev/gemini-api/docs/quickstart",
        ),
    ]
}

fn api_key_lines() -> Vec<String> {
    // API keys are optional when the CLI tool manages its own auth (e.g. `claude login`).
    // Show ✓ when env var is set, ⚠ when missing (informational only — not a blocker).
    vec![
        env_key_line("ANTHROPIC_API_KEY", "claude"),
        env_key_line("OPENAI_API_KEY", "codex"),
        {
            if env_is_set("GOOGLE_API_KEY") || env_is_set("GEMINI_API_KEY") {
                "✓ GOOGLE_API_KEY / GEMINI_API_KEY    已设置".to_string()
            } else {
                "⚠ GOOGLE_API_KEY / GEMINI_API_KEY    未设置（gemini CLI 登录态可替代）".to_string()
            }
        },
    ]
}

fn env_key_line(var: &str, cli: &str) -> String {
    if env_is_set(var) {
        format!("✓ {var}    已设置")
    } else {
        format!("⚠ {var}    未设置（{cli} CLI 登录态可替代）")
    }
}

fn session_lines(session: &Session, health: &HealthConfig, now: DateTime<Utc>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(manager_id) = &session.manager_id {
        if let Some(manager) = session.workers.get(manager_id) {
            lines.push(format!(
                "{} manager    {}   {}  context {}",
                if manager.mcp_connected { "✓" } else { "⚠" },
                capitalize(&manager.provider),
                if manager.mcp_connected { "已连接" } else { "未连接" },
                manager
                    .context_usage_pct
                    .map(|pct| format!("{}%", (pct * 100.0).round() as u32))
                    .unwrap_or_else(|| "--".to_string())
            ));
        }
    }

    let mut workers = session
        .workers
        .values()
        .filter(|worker| worker.role == WorkerRole::Worker)
        .collect::<Vec<_>>();
    workers.sort_by_key(|worker| worker.id.clone());
    for worker in workers {
        if let Some(elapsed) = heartbeat_elapsed_seconds(worker, health, now) {
            lines.push(format!(
                "⚠ {}        {}    心跳超时 {}s  → kingdom swap {}",
                worker.id,
                capitalize(&worker.provider),
                elapsed,
                worker.id
            ));
        } else {
            lines.push(format!(
                "{} {}        {}    {}",
                if worker.mcp_connected { "✓" } else { "⚠" },
                worker.id,
                capitalize(&worker.provider),
                if worker.mcp_connected { "已连接" } else { "未连接" }
            ));
        }
    }
    lines
}

struct DaemonInfo {
    pid: u32,
    started_at: DateTime<Utc>,
    socket_path: PathBuf,
    socket_exists: bool,
    watchdog_pid: Option<u32>,
}

fn daemon_status(workspace: &Path, storage_root: &Path) -> Option<DaemonInfo> {
    let pid_path = storage_root.join("daemon.pid");
    let pid = std::fs::read_to_string(&pid_path).ok()?.trim().parse::<u32>().ok()?;
    if !process_alive(pid) {
        return None;
    }
    let started_at = std::fs::metadata(&pid_path)
        .ok()?
        .modified()
        .ok()
        .map(DateTime::<Utc>::from)?;
    let socket_path = PathBuf::from(format!("/tmp/kingdom/{}.sock", workspace_hash(workspace)));
    let watchdog_pid = std::fs::read_to_string(storage_root.join("watchdog.pid"))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|pid| process_alive(*pid));
    Some(DaemonInfo {
        pid,
        started_at,
        socket_path: socket_path.clone(),
        socket_exists: socket_path.exists(),
        watchdog_pid,
    })
}

fn process_alive(pid: u32) -> bool {
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

fn versioned_binary_line(binary: &str, version_args: &[&str], fix: &str) -> String {
    if command_success("which", &[binary]) {
        let version = command_output(binary, version_args).unwrap_or_else(|| "已安装".to_string());
        format!("✓ {binary} {version}")
    } else {
        format!("✗ {binary}    未安装  {fix}")
    }
}

fn binary_line(binary: &str, fix: &str) -> String {
    if command_success("which", &[binary]) {
        format!("✓ {binary}   已安装")
    } else {
        format!("✗ {binary}   未安装  {fix}")
    }
}

fn command_success(binary: &str, args: &[&str]) -> bool {
    Command::new(binary)
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn command_output(binary: &str, args: &[&str]) -> Option<String> {
    Command::new(binary)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                String::from_utf8_lossy(&output.stderr).trim().to_string()
            } else {
                stdout
            }
        })
}

fn env_is_set(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|value| !value.is_empty())
}

fn heartbeat_elapsed_seconds(
    worker: &Worker,
    health: &HealthConfig,
    now: DateTime<Utc>,
) -> Option<i64> {
    let threshold =
        health.heartbeat_interval_seconds as i64 * i64::from(health.heartbeat_timeout_count);
    let elapsed = now.signed_duration_since(worker.last_heartbeat?).num_seconds();
    (elapsed > threshold).then_some(elapsed)
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}

fn capitalize(provider: &str) -> String {
    let mut chars = provider.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => "Unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::WorkerStatus;
    use chrono::TimeZone;

    #[test]
    fn test_doctor_api_key_line_format() {
        // When env var is not set, line should mention CLI auth alternative and not use
        // shell-specific syntax like "set" or "$env:".
        let line = env_key_line("OPENAI_API_KEY", "codex");
        assert!(line.contains("OPENAI_API_KEY"));
        assert!(line.contains("codex"));
        assert!(!line.contains("set "));
        assert!(!line.contains("$env:"));
    }

    #[test]
    fn test_doctor_heartbeat_threshold() {
        let worker = Worker {
            id: "w1".to_string(),
            provider: "codex".to_string(),
            role: WorkerRole::Worker,
            status: WorkerStatus::Running,
            job_id: None,
            pid: None,
            pane_id: String::new(),
            mcp_connected: true,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: Some(Utc.with_ymd_and_hms(2026, 4, 6, 10, 0, 0).unwrap()),
            last_progress: None,
            permissions: vec![],
            started_at: Utc::now(),
        };
        let health = HealthConfig {
            heartbeat_interval_seconds: 30,
            heartbeat_timeout_count: 2,
            process_check_interval_seconds: 5,
            progress_timeout_minutes: 30,
        };

        assert_eq!(
            heartbeat_elapsed_seconds(
                &worker,
                &health,
                Utc.with_ymd_and_hms(2026, 4, 6, 10, 0, 59).unwrap()
            ),
            None
        );
        assert_eq!(
            heartbeat_elapsed_seconds(
                &worker,
                &health,
                Utc.with_ymd_and_hms(2026, 4, 6, 10, 1, 1).unwrap()
            ),
            Some(61)
        );
    }
}
