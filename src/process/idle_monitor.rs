use crate::config::IdleConfig;
use crate::process::launcher::ProcessLauncher;
use crate::storage::Storage;
use crate::types::{Session, WorkerStatus};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use tokio::sync::Mutex;

pub fn find_idle_workers(
    session: &Session,
    config: &IdleConfig,
    now: DateTime<Utc>,
) -> Vec<(String, u32)> {
    let timeout = chrono::Duration::minutes(config.timeout_minutes as i64);
    session
        .workers
        .values()
        .filter_map(|w| {
            if w.status != WorkerStatus::Idle {
                return None;
            }
            let pid = w.pid?;
            let last_active = w.last_heartbeat.or(Some(w.started_at))?;
            if now - last_active >= timeout {
                Some((w.id.clone(), pid))
            } else {
                None
            }
        })
        .collect()
}

pub async fn run_once(
    session: &Mutex<Session>,
    launcher: &ProcessLauncher,
    config: &IdleConfig,
    storage: &Storage,
) {
    let now = Utc::now();
    let to_terminate = {
        let s = session.lock().await;
        find_idle_workers(&s, config, now)
    };
    for (worker_id, pid) in to_terminate {
        if let Err(e) = launcher.terminate(pid, true).await {
            tracing::warn!("terminate worker {worker_id} failed: {e}");
        }
        {
            let mut s = session.lock().await;
            if let Some(w) = s.workers.get_mut(&worker_id) {
                w.status = WorkerStatus::Terminated;
            }
            let _ = storage.save_session(&s);
        }
    }
}

pub async fn idle_monitor(
    session: Arc<Mutex<Session>>,
    launcher: Arc<ProcessLauncher>,
    config: crate::config::KingdomConfig,
    storage: Arc<Storage>,
) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        run_once(&session, &launcher, &config.idle, &storage).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IdleConfig;
    use crate::types::{GitStrategy, NotificationMode, Session, Worker, WorkerRole, WorkerStatus};
    use std::collections::HashMap;

    fn idle_worker(id: &str, pid: u32, idle_since_minutes: i64) -> Worker {
        let started = Utc::now() - chrono::Duration::minutes(idle_since_minutes);
        Worker {
            id: id.to_string(),
            provider: "codex".to_string(),
            role: WorkerRole::Worker,
            status: WorkerStatus::Idle,
            job_id: None,
            pid: Some(pid),
            pane_id: String::new(),
            mcp_connected: false,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: Some(started),
            last_progress: None,
            permissions: vec![],
            started_at: started,
        }
    }

    fn make_session(workers: Vec<Worker>) -> Session {
        Session {
            id: "sess_test".to_string(),
            workspace_path: "/tmp".to_string(),
            workspace_hash: "abc123".to_string(),
            manager_id: None,
            workers: workers.into_iter().map(|w| (w.id.clone(), w)).collect(),
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
            created_at: Utc::now(),
        }
    }

    #[test]
    fn finds_overdue_idle_workers() {
        let config = IdleConfig {
            timeout_minutes: 30,
        };
        let session = make_session(vec![idle_worker("w1", 1001, 40), idle_worker("w2", 1002, 10)]);
        let result = find_idle_workers(&session, &config, Utc::now());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "w1");
    }

    #[test]
    fn skips_workers_without_pid() {
        let config = IdleConfig { timeout_minutes: 5 };
        let mut w = idle_worker("w1", 0, 60);
        w.pid = None;
        let session = make_session(vec![w]);
        let result = find_idle_workers(&session, &config, Utc::now());
        assert!(result.is_empty());
    }

    #[test]
    fn skips_running_workers() {
        let config = IdleConfig { timeout_minutes: 5 };
        let mut w = idle_worker("w1", 1001, 60);
        w.status = WorkerStatus::Running;
        let session = make_session(vec![w]);
        let result = find_idle_workers(&session, &config, Utc::now());
        assert!(result.is_empty());
    }
}
