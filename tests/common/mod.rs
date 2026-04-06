#![allow(dead_code)]

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

pub fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

pub fn copy_executable(src: &Path, dst: &Path) {
    fs::copy(src, dst).unwrap();
    let mut perms = fs::metadata(dst).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(dst, perms).unwrap();
}

pub fn kingdom_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kingdom"))
}

pub fn watchdog_bin() -> PathBuf {
    kingdom_bin().parent().unwrap().join("kingdom-watchdog")
}

pub fn set_path(cmd: &mut Command, bin_dir: &Path) {
    let old_path = std::env::var("PATH").unwrap_or_default();
    cmd.env("PATH", format!("{}:{}", bin_dir.display(), old_path));
}

pub fn run_command_with_input(mut cmd: Command, input: &str) -> Output {
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

pub fn wait_until<F>(timeout: Duration, mut predicate: F) -> bool
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

pub fn cleanup_process(pid_file: &Path) {
    if let Ok(pid_str) = fs::read_to_string(pid_file) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
    }
}

pub fn write_mock_provider(bin_dir: &Path, name: &str) {
    let script = r#"#!/bin/sh
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
storage_root = config_path.parent.parent

script_file = storage_root / f"mock_script_{worker_id}.json"
results_file = storage_root / f"mock_results_{worker_id}.jsonl"

deadline = time.time() + 10
while not script_file.exists() and time.time() < deadline:
    time.sleep(0.05)
if not script_file.exists():
    sys.exit(1)

steps = json.loads(script_file.read_text())

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(socket_path)

def send_recv(msg):
    sock.sendall((json.dumps(msg) + "\n").encode())
    buf = b""
    while True:
        chunk = sock.recv(65536)
        if not chunk:
            raise RuntimeError("socket closed")
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            parsed = json.loads(line.decode())
            if parsed.get("id") == msg.get("id"):
                return parsed

req_id = 0
for step_idx, step in enumerate(steps):
    if "sleep" in step:
        time.sleep(step["sleep"])
        continue

    tool = step["tool"]
    params = step.get("params", {})

    if tool == "kingdom.hello":
        msg = {
            "jsonrpc": "2.0",
            "id": str(req_id),
            "method": "kingdom.hello",
            "params": {
                "role": params.get("role", "worker"),
                "session_id": session_id,
                "worker_id": worker_id,
            },
        }
    else:
        msg = {
            "jsonrpc": "2.0",
            "id": str(req_id),
            "method": tool,
            "params": params,
        }

    req_id += 1
    try:
        resp = send_recv(msg)
        row = {"step": step_idx, "tool": tool, "ok": "error" not in resp, "result": resp}
    except Exception as exc:
        row = {"step": step_idx, "tool": tool, "ok": False, "result": str(exc)}

    with open(results_file, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(row) + "\n")

sock.close()
PY
"#;
    write_executable(&bin_dir.join(name), script);
}

pub fn write_counting_mock_tmux(bin_dir: &Path, tmux_log: &Path, pane_ctr: &Path) {
    write_executable(
        &bin_dir.join("tmux"),
        &format!(
            "#!/bin/sh\n\
             TMUX_LOG=\"{log}\"\n\
             echo \"$@\" >> \"$TMUX_LOG\"\n\
             PANE_CTR=\"{ctr}\"\n\
             case \"$1\" in\n\
               has-session) exit 1 ;;\n\
               new-session) exit 0 ;;\n\
               split-window|new-window)\n\
                 n=$(cat \"$PANE_CTR\" 2>/dev/null || echo 0)\n\
                 n=$((n+1))\n\
                 echo $n > \"$PANE_CTR\"\n\
                 echo \"%$n\"\n\
                 ;;\n\
               send-keys) sh -lc \"$4\" >/dev/null 2>&1 & ;;\n\
               display-message) echo \"1234\" ;;\n\
               select-pane) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            log = tmux_log.display(),
            ctr = pane_ctr.display(),
        ),
    );
}

pub fn write_reconnecting_mock_provider(bin_dir: &Path, name: &str) {
    let script = r#"#!/bin/sh
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
storage_root = config_path.parent.parent

script_file = storage_root / f"mock_script_{worker_id}.json"
results_file = storage_root / f"mock_results_{worker_id}.jsonl"

deadline = time.time() + 10
while not script_file.exists() and time.time() < deadline:
    time.sleep(0.05)
if not script_file.exists():
    sys.exit(1)

steps = json.loads(script_file.read_text())

def append_row(row):
    with open(results_file, "a", encoding="utf-8") as fh:
        fh.write(json.dumps(row) + "\n")

def connect_with_retry(path, max_attempts=10):
    for attempt in range(max_attempts):
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(path)
            return sock
        except OSError:
            wait = min(0.5 * (2 ** attempt), 4.0)
            time.sleep(wait)
    raise RuntimeError(f"could not connect to {path} after {max_attempts} attempts")

def send_recv(sock, msg):
    sock.sendall((json.dumps(msg) + "\n").encode())
    buf = b""
    while True:
        chunk = sock.recv(65536)
        if not chunk:
            raise ConnectionError("socket closed")
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            parsed = json.loads(line.decode())
            if parsed.get("id") == msg.get("id"):
                return parsed

req_id = 0
current_role = "worker"
sock = connect_with_retry(socket_path)

for step_idx, step in enumerate(steps):
    if "sleep" in step:
        time.sleep(step["sleep"])
        continue

    tool = step["tool"]
    params = step.get("params", {})
    if tool == "kingdom.hello":
        current_role = params.get("role", current_role)
        msg = {
            "jsonrpc": "2.0",
            "id": str(req_id),
            "method": "kingdom.hello",
            "params": {
                "role": current_role,
                "session_id": session_id,
                "worker_id": worker_id,
            },
        }
    else:
        msg = {
            "jsonrpc": "2.0",
            "id": str(req_id),
            "method": tool,
            "params": params,
        }
    req_id += 1

    try:
        resp = send_recv(sock, msg)
        row = {"step": step_idx, "tool": tool, "ok": "error" not in resp, "result": resp}
    except (ConnectionError, BrokenPipeError, OSError):
        try:
            sock.close()
        except Exception:
            pass
        try:
            sock = connect_with_retry(socket_path)
            hello = {
                "jsonrpc": "2.0",
                "id": str(req_id),
                "method": "kingdom.hello",
                "params": {
                    "role": current_role,
                    "session_id": session_id,
                    "worker_id": worker_id,
                },
            }
            req_id += 1
            hello_resp = send_recv(sock, hello)
            append_row(
                {
                    "step": step_idx,
                    "tool": "kingdom.hello",
                    "ok": "error" not in hello_resp,
                    "reconnect": True,
                    "result": hello_resp,
                }
            )
            if tool == "kingdom.hello":
                resp = hello_resp
            else:
                resp = send_recv(sock, msg)
            row = {"step": step_idx, "tool": tool, "ok": "error" not in resp, "result": resp}
        except Exception as exc:
            row = {"step": step_idx, "tool": tool, "ok": False, "result": str(exc)}

    append_row(row)

sock.close()
PY
"#;
    write_executable(&bin_dir.join(name), script);
}

pub fn write_mock_script(storage_root: &Path, worker_id: &str, steps: &Value) {
    let path = storage_root.join(format!("mock_script_{worker_id}.json"));
    fs::write(path, serde_json::to_string_pretty(steps).unwrap()).unwrap();
}

pub fn read_mock_results(storage_root: &Path, worker_id: &str) -> Vec<Value> {
    let path = storage_root.join(format!("mock_results_{worker_id}.jsonl"));
    if !path.exists() {
        return vec![];
    }

    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub fn read_action_log(storage: &kingdom_v2::storage::Storage) -> Vec<Value> {
    let path = storage.root.join("logs").join("action.jsonl");
    if !path.exists() {
        return vec![];
    }

    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
