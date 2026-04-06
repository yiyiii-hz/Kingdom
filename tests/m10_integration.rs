use kingdom_v2::config::KingdomConfig;
use kingdom_v2::storage::Storage;
use kingdom_v2::test_support::env_lock;
use kingdom_v2::types::{
    FailoverReason, GitStrategy, JobStatus, PendingFailoverStatus, WorkerStatus,
};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::MutexGuard;
use std::time::Duration;

#[path = "common/mod.rs"]
mod common;
use common::*;

struct IntegrationFixture {
    _temp: tempfile::TempDir,
    _env_lock: MutexGuard<'static, ()>,
    workspace: PathBuf,
    bin_dir: PathBuf,
    storage: Storage,
}

impl IntegrationFixture {
    fn new(git_repo: bool) -> Self {
        let env_lock = env_lock();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();

        copy_executable(&kingdom_bin(), &bin_dir.join("kingdom"));
        copy_executable(&watchdog_bin(), &bin_dir.join("kingdom-watchdog"));
        write_mock_tmux(&bin_dir, temp.path());
        write_mock_provider(&bin_dir, "mock-codex");
        write_mock_provider(&bin_dir, "mock-worker");

        if git_repo {
            init_git_repo(&workspace);
        } else {
            fs::create_dir_all(workspace.join("src")).unwrap();
            fs::write(workspace.join("src/lib.rs"), "pub fn seed() -> i32 { 1 }\n").unwrap();
        }

        let storage = Storage::init(&workspace).unwrap();
        let mut config = KingdomConfig::default_config();
        config.failover.connect_timeout_seconds = 5;
        config.health.heartbeat_interval_seconds = 1;
        config.health.process_check_interval_seconds = 1;
        config
            .providers
            .overrides
            .insert("codex".to_string(), bin_dir.join("mock-codex").display().to_string());
        config.providers.overrides.insert(
            "mock-worker".to_string(),
            bin_dir.join("mock-worker").display().to_string(),
        );
        // Exclude real AI CLIs from PATH lookup so tests only see mock-codex.
        config.providers.overrides.insert("claude".to_string(), "/nonexistent/claude".to_string());
        config.providers.overrides.insert("gemini".to_string(), "/nonexistent/gemini".to_string());
        fs::write(
            storage.root.join("config.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();

        Self {
            _temp: temp,
            _env_lock: env_lock,
            workspace,
            bin_dir,
            storage,
        }
    }

    fn kingdom_cmd(&self) -> Command {
        let mut cmd = Command::new(self.bin_dir.join("kingdom"));
        set_path(&mut cmd, &self.bin_dir);
        cmd.env("OPENAI_API_KEY", "test-key");
        cmd
    }

    fn up(&self, input: &str) -> std::process::Output {
        let mut cmd = self.kingdom_cmd();
        cmd.arg("up").arg(&self.workspace);
        run_command_with_input(cmd, input)
    }

    fn down_force(&self) -> std::process::Output {
        let mut cmd = self.kingdom_cmd();
        cmd.arg("down").arg(&self.workspace).arg("--force");
        run_command_with_input(cmd, "")
    }
}

fn write_mock_tmux(bin_dir: &Path, root: &Path) {
    let child_pid = root.join("tmux-child.pid");
    write_executable(
        &bin_dir.join("tmux"),
        &format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
               has-session) exit 1 ;;\n\
               new-session) exit 0 ;;\n\
               split-window) echo %1 ;;\n\
               new-window) echo %4 ;;\n\
               send-keys) sh -lc \"$4\" >/dev/null 2>&1 & echo $! > \"{}\" ;;\n\
               select-pane) exit 0 ;;\n\
               display-message) cat \"{}\" ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            child_pid.display(),
            child_pid.display(),
        ),
    );
}

fn init_git_repo(workspace: &Path) {
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(workspace.join("src/lib.rs"), "pub fn seed() -> i32 { 1 }\n").unwrap();
    assert!(
        Command::new("git")
            .args(["init", workspace.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["-C", workspace.to_str().unwrap(), "config", "user.email", "test@example.com"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["-C", workspace.to_str().unwrap(), "config", "user.name", "Kingdom Tests"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["-C", workspace.to_str().unwrap(), "add", "."])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["-C", workspace.to_str().unwrap(), "commit", "-m", "init"])
            .status()
            .unwrap()
            .success()
    );
}

fn wait_for_manager_connected(storage: &Storage) {
    assert!(wait_until(Duration::from_secs(10), || {
        storage
            .load_session()
            .unwrap()
            .and_then(|session| session.workers.get("w0").cloned())
            .map(|worker| worker.mcp_connected && worker.pid.is_some() && !worker.pane_id.is_empty())
            .unwrap_or(false)
    }));
}

fn wait_for_job_status(storage: &Storage, job_id: &str, status: JobStatus) {
    let matched = wait_until(Duration::from_secs(10), || {
        storage
            .load_session()
            .unwrap()
            .and_then(|session| session.jobs.get(job_id).cloned())
            .map(|job| job.status == status)
            .unwrap_or(false)
    });
    let current = storage
        .load_session()
        .unwrap()
        .and_then(|session| session.jobs.get(job_id).cloned())
        .map(|job| format!("{:?}", job.status))
        .unwrap_or_else(|| "missing".to_string());
    assert!(matched, "job {job_id} expected {status:?}, got {current}");
}

fn daemon_pid(storage: &Storage) -> Option<u32> {
    fs::read_to_string(storage.root.join("daemon.pid"))
        .ok()
        .and_then(|pid| pid.trim().parse().ok())
}

#[test]
fn scenario1_happy_path_single_worker() {
    let fixture = IntegrationFixture::new(true);

    write_mock_script(
        &fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 0.3},
            {"tool": "job.create", "params": {"intent": "Implement add function end to end"}},
            {"tool": "worker.create", "params": {"provider": "mock-worker"}},
            {"sleep": 1.0},
            {"tool": "worker.assign", "params": {"worker_id": "w1", "job_id": "job_001"}},
            {"sleep": 2.0},
            {"tool": "job.result", "params": {"job_id": "job_001"}}
        ]),
    );
    write_mock_script(
        &fixture.storage.root,
        "w1",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "worker"}},
            {"sleep": 1.5},
            {"tool": "job.progress", "params": {"job_id": "job_001", "note": "starting implementation"}},
            {"tool": "job.complete", "params": {"job_id": "job_001", "result_summary": "Implemented add function and verified the expected behavior."}}
        ]),
    );

    let output = fixture.up("\n\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&fixture.storage);
    assert!(wait_until(Duration::from_secs(5), || {
        read_mock_results(&fixture.storage.root, "w0")
            .iter()
            .any(|row| row["tool"] == "worker.assign")
    }));
    assert!(wait_until(Duration::from_secs(5), || {
        read_mock_results(&fixture.storage.root, "w1")
            .iter()
            .any(|row| row["tool"] == "job.complete")
    }));
    let worker_results = read_mock_results(&fixture.storage.root, "w1");
    assert!(
        worker_results
            .iter()
            .any(|row| row["tool"] == "job.complete" && row["ok"] == true),
        "{worker_results:?}"
    );
    wait_for_job_status(&fixture.storage, "job_001", JobStatus::Completed);
    assert!(wait_until(Duration::from_secs(5), || {
        read_mock_results(&fixture.storage.root, "w0")
            .iter()
            .any(|row| row["tool"] == "job.result" && row["ok"] == true)
    }));

    let session = fixture.storage.load_session().unwrap().unwrap();
    let job = &session.jobs["job_001"];
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.worker_id.as_deref(), Some("w1"));

    let log = read_action_log(&fixture.storage);
    assert!(log.iter().any(|entry| entry["action"] == "job.create"));
    assert!(log.iter().any(|entry| entry["action"] == "job.progress"));
    assert!(log.iter().any(|entry| entry["action"] == "job.complete"));

    let output = fixture.down_force();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn scenario5_session_recovery() {
    let fixture = IntegrationFixture::new(true);

    write_mock_script(
        &fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 0.3},
            {"tool": "job.create", "params": {"intent": "Implement recoverable background work"}},
            {"tool": "worker.create", "params": {"provider": "mock-worker"}},
            {"sleep": 1.0},
            {"tool": "worker.assign", "params": {"worker_id": "w1", "job_id": "job_001"}},
            {"sleep": 10.0}
        ]),
    );
    write_mock_script(
        &fixture.storage.root,
        "w1",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "worker"}},
            {"sleep": 1.5},
            {"tool": "job.progress", "params": {"job_id": "job_001", "note": "work started"}},
            {"sleep": 10.0}
        ]),
    );

    let output = fixture.up("\n\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&fixture.storage);
    assert!(wait_until(Duration::from_secs(10), || {
        read_action_log(&fixture.storage)
            .iter()
            .any(|entry| entry["action"] == "job.progress")
    }));

    let output = fixture.down_force();
    assert!(output.status.success(), "{output:?}");

    let session_after_down = fixture.storage.load_session().unwrap().unwrap();
    assert_eq!(session_after_down.jobs["job_001"].status, JobStatus::Running);

    write_mock_script(
        &fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 1.0}
        ]),
    );

    let output = fixture.up("\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&fixture.storage);

    let restored = fixture.storage.load_session().unwrap().unwrap();
    assert!(restored.jobs.contains_key("job_001"));
    assert_eq!(restored.jobs["job_001"].status, JobStatus::Running);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("session") || stdout.contains("job") || stdout.contains("检测到"));

    let output = fixture.down_force();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn scenario2_parallel_workers() {
    let fixture = IntegrationFixture::new(true);
    let tmux_log_path = fixture.storage.root.join("tmux.log");
    let pane_ctr_path = fixture.storage.root.join("pane-counter");
    write_counting_mock_tmux(&fixture.bin_dir, &tmux_log_path, &pane_ctr_path);

    write_mock_script(
        &fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 0.3},
            {"tool": "job.create", "params": {"intent": "Parallel task A with enough detail"}},
            {"tool": "job.create", "params": {"intent": "Parallel task B with enough detail"}},
            {"tool": "job.create", "params": {"intent": "Parallel task C with enough detail"}},
            {"tool": "worker.create", "params": {"provider": "mock-worker"}},
            {"tool": "worker.create", "params": {"provider": "mock-worker"}},
            {"tool": "worker.create", "params": {"provider": "mock-worker"}},
            {"sleep": 3.0},
            {"tool": "worker.assign", "params": {"worker_id": "w1", "job_id": "job_001"}},
            {"tool": "worker.assign", "params": {"worker_id": "w2", "job_id": "job_002"}},
            {"tool": "worker.assign", "params": {"worker_id": "w3", "job_id": "job_003"}},
            {"sleep": 3.0}
        ]),
    );
    for worker_id in ["w1", "w2", "w3"] {
        let worker_num = worker_id[1..].parse::<u32>().unwrap();
        let job_id = format!("job_{worker_num:03}");
        let hello_delay = 1.0 + (worker_num as f32 * 0.3);
        let complete_delay = 2.5 + (worker_num as f32 * 0.5);
        write_mock_script(
            &fixture.storage.root,
            worker_id,
            &json!([
                {"sleep": hello_delay},
                {"tool": "kingdom.hello", "params": {"role": "worker"}},
                {"sleep": complete_delay},
                {"tool": "job.progress", "params": {"job_id": job_id.clone(), "note": "working in parallel"}},
                {"tool": "job.complete", "params": {"job_id": job_id, "result_summary": "Completed parallel work with enough summary detail."}}
            ]),
        );
    }

    let output = fixture.up("\n\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&fixture.storage);
    let assigned = wait_until(Duration::from_secs(8), || {
        read_mock_results(&fixture.storage.root, "w0")
            .iter()
            .filter(|row| row["tool"] == "worker.assign" && row["ok"] == true)
            .count()
            == 3
    });
    let manager_results = read_mock_results(&fixture.storage.root, "w0");
    assert!(assigned, "{manager_results:?}");
    for worker_id in ["w1", "w2", "w3"] {
        let completed = wait_until(Duration::from_secs(12), || {
            read_mock_results(&fixture.storage.root, worker_id)
                .iter()
                .any(|row| row["tool"] == "job.complete" && row["ok"] == true)
        });
        let worker_results = read_mock_results(&fixture.storage.root, worker_id);
        assert!(completed, "{worker_id}: {worker_results:?}");
    }
    for job_id in ["job_001", "job_002", "job_003"] {
        wait_for_job_status(&fixture.storage, job_id, JobStatus::Completed);
    }

    let session = fixture.storage.load_session().unwrap().unwrap();
    for job_id in ["job_001", "job_002", "job_003"] {
        assert_eq!(session.jobs[job_id].status, JobStatus::Completed);
    }

    let panes = ["w1", "w2", "w3"]
        .iter()
        .map(|worker_id| session.workers[*worker_id].pane_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        panes.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );

    let tmux_log = fs::read_to_string(&tmux_log_path).unwrap_or_default();
    assert!(tmux_log.contains("split-window"));
    assert!(!tmux_log.contains("new-window"));

    let output = fixture.down_force();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn scenario3_job_dependency_chain() {
    let fixture = IntegrationFixture::new(true);

    write_mock_script(
        &fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 0.3},
            {"tool": "job.create", "params": {"intent": "Base task for dependency unlock"}},
            {"tool": "job.create", "params": {"intent": "Step 2 depends on base", "depends_on": ["job_001"]}},
            {"tool": "job.create", "params": {"intent": "Step 3 depends on both", "depends_on": ["job_001", "job_002"]}},
            {"tool": "worker.create", "params": {"provider": "mock-worker"}},
            {"sleep": 1.0},
            {"tool": "worker.assign", "params": {"worker_id": "w1", "job_id": "job_001"}},
            {"sleep": 3.0}
        ]),
    );
    write_mock_script(
        &fixture.storage.root,
        "w1",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "worker"}},
            {"sleep": 1.5},
            {"tool": "job.complete", "params": {"job_id": "job_001", "result_summary": "Completed the base task and unlocked downstream work."}}
        ]),
    );

    let output = fixture.up("\n\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&fixture.storage);
    wait_for_job_status(&fixture.storage, "job_001", JobStatus::Completed);

    let session = fixture.storage.load_session().unwrap().unwrap();
    assert_eq!(session.jobs["job_001"].status, JobStatus::Completed);
    assert_eq!(session.jobs["job_002"].status, JobStatus::Pending);
    assert_eq!(session.jobs["job_003"].status, JobStatus::Waiting);

    let output = fixture.down_force();
    assert!(output.status.success(), "{output:?}");
    drop(fixture);

    let cancel_fixture = IntegrationFixture::new(true);
    write_mock_script(
        &cancel_fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 0.2},
            {"tool": "job.create", "params": {"intent": "Root job for cancellation cascade"}},
            {"tool": "job.create", "params": {"intent": "Child waiting on root", "depends_on": ["job_001"]}},
            {"tool": "job.cancel", "params": {"job_id": "job_001"}},
            {"sleep": 0.8}
        ]),
    );

    let output = cancel_fixture.up("\n\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&cancel_fixture.storage);
    assert!(wait_until(Duration::from_secs(5), || {
        read_action_log(&cancel_fixture.storage).iter().any(|entry| {
            entry["action"] == "job.cancel" && entry["params"]["warning"] == "cancel_cascade"
        })
    }));

    let log = read_action_log(&cancel_fixture.storage);
    let cascade_entry = log.iter().find(|entry| {
        entry["action"] == "job.cancel" && entry["params"]["warning"] == "cancel_cascade"
    });
    assert!(cascade_entry.is_some());
    let affected = cascade_entry.unwrap()["params"]["affected_jobs"]
        .as_array()
        .unwrap();
    assert!(affected.iter().any(|job| job == "job_002"));

    let output = cancel_fixture.down_force();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn scenario4_failover_context_limit() {
    let fixture = IntegrationFixture::new(true);

    write_mock_script(
        &fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 0.3},
            {"tool": "job.create", "params": {"intent": "Context limit failover trigger job"}},
            {"tool": "worker.create", "params": {"provider": "mock-worker"}},
            {"sleep": 1.5},
            {"tool": "worker.assign", "params": {"worker_id": "w1", "job_id": "job_001"}},
            {"sleep": 3.0}
        ]),
    );
    write_mock_script(
        &fixture.storage.root,
        "w1",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "worker"}},
            {"sleep": 1.8},
            {"tool": "context.ping", "params": {"usage_pct": 0.91, "token_count": 9100}},
            {"sleep": 1.0}
        ]),
    );

    let output = fixture.up("\n\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&fixture.storage);
    assert!(wait_until(Duration::from_secs(5), || {
        read_mock_results(&fixture.storage.root, "w0")
            .iter()
            .any(|row| row["tool"] == "worker.assign")
    }));
    assert!(wait_until(Duration::from_secs(5), || {
        read_mock_results(&fixture.storage.root, "w1")
            .iter()
            .any(|row| row["tool"] == "context.ping")
    }));
    let worker_results = read_mock_results(&fixture.storage.root, "w1");
    assert!(
        worker_results
            .iter()
            .any(|row| row["tool"] == "context.ping" && row["ok"] == true),
        "{worker_results:?}"
    );
    assert!(wait_until(Duration::from_secs(10), || {
        fixture
            .storage
            .load_session()
            .unwrap()
            .and_then(|session| session.pending_failovers.get("w1").cloned())
            .map(|pending| {
                pending.reason == FailoverReason::ContextLimit
                    && pending.status == PendingFailoverStatus::WaitingConfirmation
            })
            .unwrap_or(false)
    }));

    let session = fixture.storage.load_session().unwrap().unwrap();
    let pending = &session.pending_failovers["w1"];
    assert_eq!(pending.reason, FailoverReason::ContextLimit);
    assert_eq!(pending.status, PendingFailoverStatus::WaitingConfirmation);
    assert!(read_action_log(&fixture.storage)
        .iter()
        .any(|entry| entry["action"] == "failover.triggered"));

    let output = fixture.down_force();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn scenario6_daemon_crash_recovery() {
    let fixture = IntegrationFixture::new(true);
    write_reconnecting_mock_provider(&fixture.bin_dir, "mock-codex");

    write_mock_script(
        &fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 2.0},
            {"tool": "job.create", "params": {"intent": "Manager proves reconnect after daemon restart"}},
            {"sleep": 2.0}
        ]),
    );

    let output = fixture.up("\n\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&fixture.storage);

    let old_daemon_pid = daemon_pid(&fixture.storage).unwrap();
    kill(Pid::from_raw(old_daemon_pid as i32), Signal::SIGKILL).unwrap();

    assert!(wait_until(Duration::from_secs(8), || {
        daemon_pid(&fixture.storage)
            .map(|new_pid| {
                new_pid != old_daemon_pid && kill(Pid::from_raw(new_pid as i32), None).is_ok()
            })
            .unwrap_or(false)
    }));

    assert!(wait_until(Duration::from_secs(12), || {
        read_mock_results(&fixture.storage.root, "w0")
            .iter()
            .filter(|row| row["tool"] == "kingdom.hello" && row["ok"] == true)
            .count()
            >= 2
    }));

    let new_daemon_pid = daemon_pid(&fixture.storage).unwrap();
    assert_ne!(new_daemon_pid, old_daemon_pid);
    let session = fixture.storage.load_session().unwrap().unwrap();
    assert!(session.workers["w0"].mcp_connected);

    let output = fixture.down_force();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn scenario7a_non_git_dir_downgrades_strategy() {
    let fixture = IntegrationFixture::new(false);

    write_mock_script(
        &fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 1.0}
        ]),
    );

    let output = fixture.up("\n\n\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&fixture.storage);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("not a git repository"));

    let session = fixture.storage.load_session().unwrap().unwrap();
    assert_eq!(session.git_strategy, GitStrategy::None);

    let output = fixture.down_force();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn scenario7d_job_complete_idempotent() {
    let fixture = IntegrationFixture::new(true);

    write_mock_script(
        &fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 0.3},
            {"tool": "job.create", "params": {"intent": "Implement idempotent completion handling"}},
            {"tool": "worker.create", "params": {"provider": "mock-worker"}},
            {"sleep": 1.0},
            {"tool": "worker.assign", "params": {"worker_id": "w1", "job_id": "job_001"}},
            {"sleep": 2.0}
        ]),
    );
    write_mock_script(
        &fixture.storage.root,
        "w1",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "worker"}},
            {"sleep": 1.5},
            {"tool": "job.progress", "params": {"job_id": "job_001", "note": "progress before complete"}},
            {"tool": "job.complete", "params": {"job_id": "job_001", "result_summary": "Completed the task once and preserved idempotent completion semantics."}},
            {"tool": "job.complete", "params": {"job_id": "job_001", "result_summary": "Completed the task once and preserved idempotent completion semantics."}}
        ]),
    );

    let output = fixture.up("\n\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&fixture.storage);
    wait_for_job_status(&fixture.storage, "job_001", JobStatus::Completed);
    assert!(wait_until(Duration::from_secs(5), || {
        read_mock_results(&fixture.storage.root, "w1")
            .iter()
            .filter(|row| row["tool"] == "job.complete")
            .count()
            >= 2
    }));

    let results = read_mock_results(&fixture.storage.root, "w1");
    let complete_rows = results
        .iter()
        .filter(|row| row["tool"] == "job.complete")
        .collect::<Vec<_>>();
    assert_eq!(complete_rows.len(), 2);
    assert_eq!(complete_rows[1]["ok"], true);

    let session = fixture.storage.load_session().unwrap().unwrap();
    assert_eq!(session.jobs["job_001"].status, JobStatus::Completed);

    let output = fixture.down_force();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn scenario7e_release_running_worker_returns_error() {
    let fixture = IntegrationFixture::new(true);

    write_mock_script(
        &fixture.storage.root,
        "w0",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "manager"}},
            {"sleep": 0.3},
            {"tool": "job.create", "params": {"intent": "Attempt release of a running worker"}},
            {"tool": "worker.create", "params": {"provider": "mock-worker"}},
            {"sleep": 1.0},
            {"tool": "worker.assign", "params": {"worker_id": "w1", "job_id": "job_001"}},
            {"sleep": 0.2},
            {"tool": "worker.release", "params": {"worker_id": "w1"}},
            {"sleep": 1.0}
        ]),
    );
    write_mock_script(
        &fixture.storage.root,
        "w1",
        &json!([
            {"tool": "kingdom.hello", "params": {"role": "worker"}},
            {"sleep": 3.0}
        ]),
    );

    let output = fixture.up("\n\n");
    assert!(output.status.success(), "{output:?}");
    wait_for_manager_connected(&fixture.storage);
    assert!(wait_until(Duration::from_secs(5), || {
        read_mock_results(&fixture.storage.root, "w0")
            .iter()
            .any(|row| row["tool"] == "worker.release")
    }));

    let results = read_mock_results(&fixture.storage.root, "w0");
    assert!(results
        .iter()
        .any(|row| row["tool"] == "worker.release" && row["ok"] == false));

    let session = fixture.storage.load_session().unwrap().unwrap();
    assert_eq!(session.workers["w1"].status, WorkerStatus::Running);

    let output = fixture.down_force();
    assert!(output.status.success(), "{output:?}");
}
