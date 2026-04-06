use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub async fn run_daemon(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or_else(|_| workspace.clone());
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

    let dispatcher = Arc::new(crate::mcp::dispatcher::Dispatcher::for_daemon(
        Arc::clone(&storage),
        Arc::clone(&push),
        Arc::clone(&notifications),
        Arc::clone(&health_events),
        Arc::clone(&awaiters),
        Arc::clone(&launcher),
    ));

    let server = crate::mcp::server::McpServer::with_dispatcher(
        &hash,
        Arc::clone(&storage),
        dispatcher,
        Arc::clone(&push),
    );
    server.start().await?;

    tokio::spawn(crate::config::watcher::config_watcher(
        config_path,
        Arc::clone(&config_arc),
    ));

    if let Some(session) = storage.load_session()? {
        let session = Arc::new(Mutex::new(session));
        let idle_launcher = Arc::clone(&launcher);
        let idle_storage = Arc::clone(&storage);
        tokio::spawn(crate::process::idle_monitor::idle_monitor(
            session,
            idle_launcher,
            Arc::clone(&config_arc),
            idle_storage,
        ));
    }

    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?
        .recv()
        .await;

    server.stop().await?;
    let _ = std::fs::remove_file(storage_root.join("daemon.pid"));
    Ok(())
}
