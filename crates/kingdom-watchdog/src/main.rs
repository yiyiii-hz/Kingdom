use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
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
        let mut child: Child = match Command::new(&kingdom_bin)
            .arg("daemon")
            .arg(&workspace)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[watchdog] failed to start daemon: {e}");
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
                    eprintln!("[watchdog] daemon exited ({status:?}), restarting in 1s...");
                    std::thread::sleep(Duration::from_secs(1));
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(e) => {
                    eprintln!("[watchdog] failed to poll daemon: {e}");
                    std::thread::sleep(Duration::from_secs(1));
                    break;
                }
            }
        }
    }
}
