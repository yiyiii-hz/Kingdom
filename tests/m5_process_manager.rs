use chrono::Utc;
use kingdom_v2::config::KingdomConfig;
use kingdom_v2::storage::Storage;
use kingdom_v2::types::{GitStrategy, JobStatus, NotificationMode, Session};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

fn copy_executable(src: &Path, dst: &Path) {
    fs::copy(src, dst).unwrap();
    let mut perms = fs::metadata(dst).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(dst, perms).unwrap();
}

fn kingdom_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kingdom"))
}

fn watchdog_bin() -> PathBuf {
    kingdom_bin().parent().unwrap().join("kingdom-watchdog")
}

fn fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();

    let kingdom_copy = bin_dir.join("kingdom");
    copy_executable(&kingdom_bin(), &kingdom_copy);

    let tmux_log = tmp.path().join("tmux.log");
    write_executable(
        &bin_dir.join("tmux"),
        &format!(
            "#!/bin/sh\n\
             echo \"$@\" >> \"{}\"\n\
             case \"$1\" in\n\
               has-session) exit 1 ;;\n\
               new-session) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            tmux_log.display()
        ),
    );

    (tmp, workspace, bin_dir, tmux_log)
}

fn default_session(workspace: &Path) -> Session {
    Session {
        id: "sess_test".to_string(),
        workspace_path: workspace.display().to_string(),
        workspace_hash: kingdom_v2::config::workspace_hash(workspace),
        manager_id: None,
        workers: HashMap::new(),
        jobs: HashMap::new(),
        notes: vec![],
        worker_seq: 0,
        job_seq: 0,
        request_seq: 0,
        git_strategy: GitStrategy::None,
        available_providers: vec![],
        notification_mode: NotificationMode::Poll,
        pending_requests: HashMap::new(),
        pending_failovers: HashMap::new(),
        provider_stability: HashMap::new(),
        created_at: Utc::now(),
    }
}

fn run_command_with_input(mut cmd: Command, input: &str) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if !input.is_empty() {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn set_path(cmd: &mut Command, bin_dir: &Path) {
    let old_path = std::env::var("PATH").unwrap_or_default();
    cmd.env("PATH", format!("{}:{}", bin_dir.display(), old_path));
}

fn wait_until<F>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    predicate()
}

#[test]
fn kingdom_up_generates_kingdom_md_and_pid_files() {
    let (_tmp, workspace, bin_dir, tmux_log) = fixture();
    Command::new("git")
        .args(["init", workspace.to_str().unwrap()])
        .output()
        .unwrap();

    let provider = bin_dir.join("codex-provider");
    write_executable(&provider, "#!/bin/sh\nexit 0\n");

    let mut cfg = KingdomConfig::default_config();
    cfg.providers
        .overrides
        .insert("codex".to_string(), provider.display().to_string());

    let storage = Storage::init(&workspace).unwrap();
    fs::write(
        storage.root.join("config.toml"),
        toml::to_string(&cfg).unwrap(),
    )
    .unwrap();

    write_executable(
        &bin_dir.join("kingdom-watchdog"),
        &format!(
            "#!/bin/sh\n\
             mkdir -p \"{root}\"\n\
             echo $$ > \"{root}/watchdog.pid\"\n\
             echo 12345 > \"{root}/daemon.pid\"\n\
             sleep 2\n",
            root = storage.root.display(),
        ),
    );

    let mut cmd = Command::new(bin_dir.join("kingdom"));
    cmd.arg("up").arg(&workspace);
    set_path(&mut cmd, &bin_dir);
    cmd.env("OPENAI_API_KEY", "test-key");
    let output = run_command_with_input(cmd, "\n\n");
    assert!(output.status.success(), "{:?}", output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Created KINGDOM.md"));
    assert!(stdout.contains("Kingdom started."));
    assert!(workspace.join("KINGDOM.md").exists());
    assert!(storage.root.join("daemon.pid").exists());
    assert!(storage.root.join("watchdog.pid").exists());
    let session = storage.load_session().unwrap().unwrap();
    assert_eq!(session.manager_id.as_deref(), Some("w0"));
    assert_eq!(session.workers["w0"].provider, "codex");
    assert!(session.available_providers.contains(&"codex".to_string()));

    let tmux = fs::read_to_string(tmux_log).unwrap();
    assert!(tmux.contains("new-session"));
}

#[test]
fn kingdom_up_existing_session_prints_attach_hint() {
    let (_tmp, workspace, bin_dir, _tmux_log) = fixture();
    Command::new("git")
        .args(["init", workspace.to_str().unwrap()])
        .output()
        .unwrap();

    let provider = bin_dir.join("codex-provider");
    write_executable(&provider, "#!/bin/sh\nexit 0\n");

    let mut cfg = KingdomConfig::default_config();
    cfg.tmux.session_name = "kingdom-test".to_string();
    cfg.providers
        .overrides
        .insert("codex".to_string(), provider.display().to_string());

    let storage = Storage::init(&workspace).unwrap();
    fs::write(
        storage.root.join("config.toml"),
        toml::to_string(&cfg).unwrap(),
    )
    .unwrap();
    fs::write(
        storage.root.join("daemon.pid"),
        format!("{}\n", std::process::id()),
    )
    .unwrap();

    let mut cmd = Command::new(bin_dir.join("kingdom"));
    cmd.arg("up").arg(&workspace);
    set_path(&mut cmd, &bin_dir);
    cmd.env("OPENAI_API_KEY", "test-key");
    let output = run_command_with_input(cmd, "");
    assert!(output.status.success(), "{:?}", output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("already running"));
    assert!(stdout.contains("tmux attach -t kingdom-test"));
}

#[test]
fn kingdom_up_in_non_git_directory_can_abort() {
    let (_tmp, workspace, bin_dir, _tmux_log) = fixture();

    let mut cmd = Command::new(bin_dir.join("kingdom"));
    cmd.arg("up").arg(&workspace);
    set_path(&mut cmd, &bin_dir);
    let output = run_command_with_input(cmd, "n\n");
    assert!(!output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("not a git repository"));
    assert!(stderr.contains("Aborted"));
}

#[test]
fn kingdom_up_fails_when_no_manager_provider_available() {
    let (_tmp, workspace, bin_dir, _tmux_log) = fixture();
    Command::new("git")
        .args(["init", workspace.to_str().unwrap()])
        .output()
        .unwrap();

    let storage = Storage::init(&workspace).unwrap();
    fs::write(
        storage.root.join("config.toml"),
        toml::to_string(&KingdomConfig::default_config()).unwrap(),
    )
    .unwrap();

    let mut cmd = Command::new(bin_dir.join("kingdom"));
    cmd.arg("up").arg(&workspace);
    set_path(&mut cmd, &bin_dir);
    cmd.env_remove("OPENAI_API_KEY");
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("GEMINI_API_KEY");
    let output = run_command_with_input(cmd, "");
    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No providers available"));
}

#[test]
fn kingdom_down_force_kills_daemon_and_watchdog_processes() {
    let (_tmp, workspace, bin_dir, _tmux_log) = fixture();
    let storage = Storage::init(&workspace).unwrap();
    storage.save_session(&default_session(&workspace)).unwrap();

    let mut daemon = Command::new("sleep").arg("30").spawn().unwrap();
    let mut watchdog = Command::new("sleep").arg("30").spawn().unwrap();
    fs::write(
        storage.root.join("daemon.pid"),
        format!("{}\n", daemon.id()),
    )
    .unwrap();
    fs::write(
        storage.root.join("watchdog.pid"),
        format!("{}\n", watchdog.id()),
    )
    .unwrap();

    let mut cmd = Command::new(bin_dir.join("kingdom"));
    cmd.arg("down").arg(&workspace).arg("--force");
    set_path(&mut cmd, &bin_dir);
    let output = run_command_with_input(cmd, "");
    assert!(output.status.success(), "{:?}", output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Kingdom stopped."));
    assert!(!storage.root.join("daemon.pid").exists());
    assert!(!storage.root.join("watchdog.pid").exists());
    let _ = daemon.wait();
    let _ = watchdog.wait();
}

#[test]
fn kingdom_down_with_running_jobs_shows_three_options() {
    let (_tmp, workspace, bin_dir, _tmux_log) = fixture();
    let storage = Storage::init(&workspace).unwrap();
    let mut session = default_session(&workspace);
    session.jobs.insert(
        "job_001".to_string(),
        kingdom_v2::types::Job {
            id: "job_001".to_string(),
            intent: "test".to_string(),
            status: JobStatus::Running,
            worker_id: None,
            depends_on: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            branch: None,
            branch_start_commit: None,
            checkpoints: vec![],
            result: None,
            fail_count: 0,
            last_fail_at: None,
        },
    );
    storage.save_session(&session).unwrap();

    let mut cmd = Command::new(bin_dir.join("kingdom"));
    cmd.arg("down").arg(&workspace).arg("--force");
    set_path(&mut cmd, &bin_dir);
    let output = run_command_with_input(cmd, "");
    assert!(output.status.success(), "{:?}", output);

    let mut interactive = Command::new(bin_dir.join("kingdom"));
    interactive.arg("down").arg(&workspace);
    set_path(&mut interactive, &bin_dir);
    let output = run_command_with_input(interactive, "3\n");
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Wait for completion"));
    assert!(stdout.contains("Suspend and exit"));
    assert!(stdout.contains("Force quit now"));
}

#[test]
fn watchdog_restarts_failed_daemon_and_exits_on_sigterm() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(workspace.join(".kingdom")).unwrap();
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();

    let watchdog_copy = bin_dir.join("kingdom-watchdog");
    copy_executable(&watchdog_bin(), &watchdog_copy);
    write_executable(
        &bin_dir.join("kingdom"),
        r#"#!/bin/sh
workspace="$2"
root="$workspace/.kingdom"
mkdir -p "$root"
count_file="$root/daemon-count"
count=0
if [ -f "$count_file" ]; then
  count=$(cat "$count_file")
fi
count=$((count + 1))
echo "$count" > "$count_file"
echo $$ > "$root/daemon.pid"
if [ "$count" -eq 1 ]; then
  exit 1
fi
trap 'rm -f "$root/daemon.pid"; exit 0' TERM
while true; do
  sleep 1
done
"#,
    );

    let mut child = Command::new(&watchdog_copy)
        .arg(&workspace)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let root = workspace.join(".kingdom");
    assert!(wait_until(Duration::from_secs(3), || {
        root.join("watchdog.pid").exists()
    }));
    assert!(wait_until(Duration::from_secs(6), || {
        fs::read_to_string(root.join("daemon-count"))
            .ok()
            .as_deref()
            == Some("2\n")
            || fs::read_to_string(root.join("daemon-count"))
                .ok()
                .as_deref()
                == Some("2")
    }));

    let pid = fs::read_to_string(root.join("watchdog.pid"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    kill(Pid::from_raw(pid), Signal::SIGTERM).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
    assert!(wait_until(Duration::from_secs(2), || {
        !root.join("watchdog.pid").exists()
    }));
}

#[test]
fn daemon_command_writes_daemon_pid_file() {
    let (_tmp, workspace, bin_dir, _tmux_log) = fixture();
    let mut cmd = Command::new(bin_dir.join("kingdom"));
    cmd.arg("daemon").arg(&workspace);
    set_path(&mut cmd, &bin_dir);
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let storage_root = workspace.join(".kingdom");
    assert!(wait_until(Duration::from_secs(2), || {
        storage_root.join("daemon.pid").exists()
    }));

    let pid = fs::read_to_string(storage_root.join("daemon.pid")).unwrap();
    assert!(pid.trim().parse::<u32>().is_ok());

    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}
