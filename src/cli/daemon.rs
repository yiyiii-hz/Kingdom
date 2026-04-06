use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::types::{ActionLogEntry, WorkerStatus};

pub async fn run_daemon(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());
    let storage = Arc::new(crate::storage::Storage::init(&workspace)?);
    let storage_root = storage.root.clone();

    let pid = std::process::id();
    std::fs::write(storage_root.join("daemon.pid"), format!("{pid}\n"))?;

    let hash = crate::config::workspace_hash(&workspace);
    let config_path = storage_root.join("config.toml");
    let config = crate::config::KingdomConfig::load_or_default(&config_path);
    let config_arc = Arc::new(RwLock::new(config.clone()));

    let launcher = Arc::new(crate::process::launcher::ProcessLauncher::new(
        workspace.clone(),
        config.clone(),
        hash.clone(),
    ));

    let push = Arc::new(RwLock::new(crate::mcp::push::PushRegistry::new()));
    let notifications = Arc::new(Mutex::new(crate::mcp::queues::NotificationQueue::new()));
    let health_events = Arc::new(Mutex::new(crate::mcp::queues::HealthEventQueue::new()));
    let awaiters = Arc::new(Mutex::new(crate::mcp::queues::RequestAwaiterRegistry::new()));
    let (failover_tx, failover_rx) = tokio::sync::mpsc::channel(32);
    let cli_failover_tx = failover_tx.clone();

    let dispatcher = Arc::new(crate::mcp::dispatcher::Dispatcher::for_daemon(
        Arc::clone(&storage),
        Arc::clone(&push),
        Arc::clone(&notifications),
        Arc::clone(&health_events),
        Arc::clone(&awaiters),
        Arc::clone(&launcher),
        failover_tx,
    ));

    let server = crate::mcp::server::McpServer::with_dispatcher(
        &hash,
        Arc::clone(&storage),
        dispatcher,
        Arc::clone(&push),
    );
    server.start().await?;
    let cli_server = crate::mcp::cli_server::CliServer::new(
        &hash,
        workspace.clone(),
        Arc::clone(&storage),
        cli_failover_tx,
        Arc::clone(&push),
    );
    cli_server.start().await?;

    tokio::spawn(crate::config::watcher::config_watcher(
        config_path,
        Arc::clone(&config_arc),
    ));

    if let Some(session) = storage.load_session()? {
        ensure_manager_started(&storage, &launcher, &config_arc).await;
        let session = Arc::new(Mutex::new(session));
        let idle_launcher = Arc::clone(&launcher);
        let idle_storage = Arc::clone(&storage);
        tokio::spawn(crate::process::idle_monitor::idle_monitor(
            Arc::clone(&session),
            idle_launcher,
            Arc::clone(&config_arc),
            idle_storage,
        ));

        let (health_tx, health_rx) = tokio::sync::mpsc::channel(64);
        let health_monitor = crate::health::monitor::HealthMonitor::new(
            Arc::clone(&session),
            config.clone().health,
            health_tx,
            Arc::clone(&push),
            Arc::clone(&health_events),
            Arc::clone(&storage),
        );
        tokio::spawn(async move {
            health_monitor.run().await;
        });

        let failover_machine = crate::failover::machine::FailoverMachine::new(
            Arc::clone(&storage),
            Arc::clone(&config_arc),
            Arc::clone(&notifications),
            health_rx,
            failover_rx,
            Arc::clone(&launcher),
        );
        tokio::spawn(async move {
            failover_machine.run().await;
        });

        let sync_session = Arc::clone(&session);
        let sync_storage = Arc::clone(&storage);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if let Ok(Some(latest)) = sync_storage.load_session() {
                    *sync_session.lock().await = latest;
                }
            }
        });
    }

    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?
        .recv()
        .await;

    server.stop().await?;
    let _ = std::fs::remove_file(storage_root.join("daemon.pid"));
    Ok(())
}

pub async fn ensure_manager_started(
    storage: &Arc<crate::storage::Storage>,
    launcher: &Arc<crate::process::launcher::ProcessLauncher>,
    config: &Arc<RwLock<crate::config::KingdomConfig>>,
) {
    let Ok(Some(mut session)) = storage.load_session() else {
        return;
    };
    let Some(manager_id) = session.manager_id.clone() else {
        return;
    };
    let Some(manager) = session.workers.get(&manager_id).cloned() else {
        return;
    };

    if manager.mcp_connected && manager.pid.is_some() && !manager.pane_id.is_empty() {
        return;
    }

    let worker_index = session
        .workers
        .keys()
        .position(|worker_id| worker_id == &manager_id)
        .unwrap_or(0);
    let launched = launcher
        .launch(
            &manager.provider,
            manager.role.clone(),
            &manager_id,
            worker_index,
            &storage.root,
        )
        .await;

    match launched {
        Ok(launch) => {
            if let Some(worker) = session.workers.get_mut(&manager_id) {
                worker.pid = Some(launch.pid);
                worker.pane_id = launch.pane_id;
                worker.mcp_connected = false;
                worker.status = WorkerStatus::Starting;
                worker.last_heartbeat = None;
                worker.last_progress = None;
                worker.started_at = chrono::Utc::now();
            }
            let _ = storage.save_session(&session);

            let timeout = config.read().await.failover.connect_timeout_seconds;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
            loop {
                if tokio::time::Instant::now() > deadline {
                    mark_manager_failed(
                        storage,
                        &manager_id,
                        format!(
                            "manager provider '{}' did not connect in {timeout}s",
                            manager.provider
                        ),
                    );
                    return;
                }

                let Ok(Some(current)) = storage.load_session() else {
                    return;
                };
                let Some(current_manager) = current.workers.get(&manager_id) else {
                    return;
                };
                if current_manager.mcp_connected {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        Err(error) => {
            mark_manager_failed(storage, &manager_id, error.to_string());
        }
    }
}

fn mark_manager_failed(storage: &Arc<crate::storage::Storage>, manager_id: &str, error: String) {
    let Ok(Some(mut session)) = storage.load_session() else {
        return;
    };
    if let Some(worker) = session.workers.get_mut(manager_id) {
        worker.status = WorkerStatus::Failed;
        worker.mcp_connected = false;
        worker.last_progress = None;
        worker.last_heartbeat = None;
    }
    let _ = storage.save_session(&session);
    let _ = storage.append_action_log(&ActionLogEntry {
        timestamp: chrono::Utc::now(),
        actor: "kingdom-daemon".to_string(),
        action: "manager.start_failed".to_string(),
        params: serde_json::json!({ "worker_id": manager_id }),
        result: None,
        error: Some(error),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KingdomConfig;
    use crate::process::launcher::ProcessLauncher;
    use crate::test_support::env_lock;
    use crate::types::{GitStrategy, NotificationMode, Session, Worker, WorkerRole, WorkerStatus};
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn session(workspace: &std::path::Path) -> Session {
        Session {
            id: "sess_1".to_string(),
            workspace_path: workspace.display().to_string(),
            workspace_hash: "hash123".to_string(),
            manager_id: Some("w0".to_string()),
            workers: [(
                "w0".to_string(),
                Worker {
                    id: "w0".to_string(),
                    provider: "codex".to_string(),
                    role: WorkerRole::Manager,
                    status: WorkerStatus::Starting,
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
            jobs: HashMap::new(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: GitStrategy::None,
            available_providers: vec!["codex".to_string()],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: chrono::Utc::now(),
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn ensure_manager_started_marks_failure_when_launch_fails() {
        let _env_lock = env_lock();
        let temp = tempdir().unwrap();
        let storage = Arc::new(crate::storage::Storage::init(temp.path()).unwrap());
        storage.save_session(&session(temp.path())).unwrap();

        let mut config = KingdomConfig::default_config();
        config.providers.overrides.insert(
            "codex".to_string(),
            temp.path().join("missing-provider").display().to_string(),
        );
        let launcher = Arc::new(ProcessLauncher::new(
            temp.path().to_path_buf(),
            config.clone(),
            "hash123".to_string(),
        ));
        let config = Arc::new(RwLock::new(config));

        ensure_manager_started(&storage, &launcher, &config).await;

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.workers["w0"].status, WorkerStatus::Failed);
        let entries = storage.read_action_log(Some(5)).unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry.action == "manager.start_failed"));
    }
}
