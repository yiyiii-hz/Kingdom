use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigterm(_: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn main() {
    let workspace = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let storage_root = workspace.join(".kingdom");
    let _ = std::fs::create_dir_all(&storage_root);
    let log_path = storage_root.join("watchdog.log");
    log_message(
        &log_path,
        &format!("[watchdog] starting for workspace {}", workspace.display()),
    );
    let pid = std::process::id();
    let _ = std::fs::write(storage_root.join("watchdog.pid"), format!("{pid}\n"));

    unsafe {
        let _ = nix::sys::signal::signal(
            Signal::SIGTERM,
            nix::sys::signal::SigHandler::Handler(handle_sigterm),
        );
    }

    let kingdom_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("kingdom")))
        .unwrap_or_else(|| PathBuf::from("kingdom"));

    loop {
        log_message(&log_path, "[watchdog] spawning daemon");
        let mut child: Child = match Command::new(&kingdom_bin)
            .arg("daemon")
            .arg(&workspace)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                log_message(
                    &log_path,
                    &format!("[watchdog] failed to start daemon: {e}"),
                );
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        loop {
            if SHUTDOWN.load(Ordering::SeqCst) {
                let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
                let _ = child.wait();
                let _ = std::fs::remove_file(storage_root.join("watchdog.pid"));
                return;
            }

            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.code() == Some(0) {
                        let _ = std::fs::remove_file(storage_root.join("watchdog.pid"));
                        return;
                    }
                    log_message(
                        &log_path,
                        &format!("[watchdog] daemon exited ({status:?}), restarting in 1s..."),
                    );
                    std::thread::sleep(Duration::from_secs(1));
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(e) => {
                    log_message(&log_path, &format!("[watchdog] failed to poll daemon: {e}"));
                    std::thread::sleep(Duration::from_secs(1));
                    break;
                }
            }
        }
    }
}

fn log_message(log_path: &std::path::Path, message: &str) {
    use std::time::{SystemTime, UNIX_EPOCH};

    eprintln!("{message}");

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{ts}] {message}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .and_then(|mut file| file.write_all(line.as_bytes()));
}
