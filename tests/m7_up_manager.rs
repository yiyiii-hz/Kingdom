use kingdom_v2::config::KingdomConfig;
use kingdom_v2::storage::Storage;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

fn set_path(cmd: &mut Command, bin_dir: &Path) {
    let old_path = std::env::var("PATH").unwrap_or_default();
    cmd.env("PATH", format!("{}:{}", bin_dir.display(), old_path));
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

fn wait_until<F>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    predicate()
}

fn cleanup_process(pid_file: &Path) {
    if let Ok(pid_str) = fs::read_to_string(pid_file) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
    }
}

#[test]
fn kingdom_up_bootstraps_manager_provider_and_waits_for_connection() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    Command::new("git")
        .args(["init", workspace.to_str().unwrap()])
        .output()
        .unwrap();

    copy_executable(&kingdom_bin(), &bin_dir.join("kingdom"));
    copy_executable(&watchdog_bin(), &bin_dir.join("kingdom-watchdog"));

    let tmux_child_pid = temp.path().join("tmux-child.pid");
    write_executable(
        &bin_dir.join("tmux"),
        &format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
               has-session) exit 1 ;;\n\
               new-session) exit 0 ;;\n\
               split-window) echo %1 ;;\n\
               send-keys) sh -lc \"$4\" >/dev/null 2>&1 & echo $! > \"{}\" ;;\n\
               display-message) cat \"{}\" ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            tmux_child_pid.display(),
            tmux_child_pid.display(),
        ),
    );

    let provider = bin_dir.join("codex-provider");
    write_executable(
        &provider,
        r#"#!/bin/sh
CONFIG=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--mcp-config" ]; then
    CONFIG="$2"
    shift 2
  else
    shift
  fi
done
python3 - "$CONFIG" <<'PY'
import json
import pathlib
import socket
import sys
import time

config_path = pathlib.Path(sys.argv[1]).resolve()
config = json.loads(config_path.read_text())
socket_path = config["mcpServers"]["kingdom"]["args"][0].split("UNIX-CONNECT:", 1)[1]
worker_id = config_path.stem
state = json.loads((config_path.parent.parent / "state.json").read_text())
session_id = state["session_id"]

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(socket_path)
hello = {
    "jsonrpc": "2.0",
    "id": "init",
    "method": "kingdom.hello",
    "params": {
        "role": "manager",
        "session_id": session_id,
        "worker_id": worker_id,
    },
}
sock.sendall((json.dumps(hello) + "\n").encode())
sock.recv(4096)
time.sleep(5)
PY
"#,
    );

    let storage = Storage::init(&workspace).unwrap();
    let mut cfg = KingdomConfig::default_config();
    cfg.failover.connect_timeout_seconds = 5;
    cfg.providers
        .overrides
        .insert("codex".to_string(), provider.display().to_string());
    fs::write(
        storage.root.join("config.toml"),
        toml::to_string(&cfg).unwrap(),
    )
    .unwrap();

    let mut cmd = Command::new(bin_dir.join("kingdom"));
    cmd.arg("up").arg(&workspace);
    set_path(&mut cmd, &bin_dir);
    cmd.env("OPENAI_API_KEY", "test-key");
    let output = run_command_with_input(cmd, "\n\n");
    assert!(output.status.success(), "{output:?}");

    assert!(wait_until(Duration::from_secs(5), || {
        storage
            .load_session()
            .unwrap()
            .and_then(|session| session.workers.get("w0").cloned())
            .map(|worker| {
                worker.mcp_connected && worker.pid.is_some() && !worker.pane_id.is_empty()
            })
            .unwrap_or(false)
    }));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("manager pid:"));
    assert!(stdout.contains("manager pane:"));

    let session = storage.load_session().unwrap().unwrap();
    let manager = &session.workers["w0"];
    assert_eq!(manager.provider, "codex");
    assert!(manager.pid.is_some());
    assert!(manager.mcp_connected);
    assert!(!manager.pane_id.is_empty());

    cleanup_process(&storage.root.join("watchdog.pid"));
    cleanup_process(&storage.root.join("daemon.pid"));
}
