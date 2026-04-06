use crate::failover::handoff::build_handoff_brief;
use crate::failover::machine::FailoverCommand;
use crate::failover::recommender::recommend_provider;
use crate::cli::daemon_client::{send_cli_command, socket_path};
use crate::mcp::push::PushRegistry;
use crate::storage::Storage;
use crate::types::{
    ActionLogEntry, CheckpointMeta, FailoverReason, PendingFailover, PendingFailoverStatus,
    WorkerRole,
};
use chrono::Utc;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

pub async fn run_swap(
    workspace: PathBuf,
    worker_id: String,
    provider: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Arc::new(Storage::init(&workspace)?);
    let provider = match provider {
        Some(provider) => Some(provider),
        None => prompt_provider_selection(&storage, &worker_id)?,
    };
    if try_swap_via_daemon(&workspace, &worker_id, provider.clone()).await? {
        println!("queued manual swap for worker");
        return Ok(());
    }
    queue_manual_swap(&storage, &workspace, &worker_id, provider, None, None).await?;
    println!("queued manual swap for worker");
    Ok(())
}

fn prompt_provider_selection(
    storage: &Storage,
    worker_id: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    use std::io::Write;

    let session = match storage.load_session()? {
        Some(session) => session,
        None => return Ok(None),
    };
    let current_provider = session
        .workers
        .get(worker_id)
        .map(|worker| worker.provider.as_str())
        .unwrap_or("");
    let candidates = available_swap_candidates(&session, current_provider);
    if candidates.is_empty() {
        println!("没有其他可用 provider，将由系统自动推荐。");
        return Ok(None);
    }

    println!("选择目标 provider（当前：{current_provider}）：");
    for (index, provider) in candidates.iter().enumerate() {
        println!("  {}) {}", index + 1, provider);
    }
    print!("输入编号 [1]: ");
    std::io::stdout().flush()?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let idx = line.trim().parse::<usize>().unwrap_or(1).saturating_sub(1);
    let chosen = candidates.get(idx).copied().unwrap_or(candidates[0]);
    Ok(Some(chosen.to_string()))
}

fn available_swap_candidates<'a>(
    session: &'a crate::types::Session,
    current_provider: &str,
) -> Vec<&'a str> {
    session
        .available_providers
        .iter()
        .map(|provider| provider.as_str())
        .filter(|provider| *provider != current_provider)
        .collect()
}

pub async fn queue_manual_swap(
    storage: &Arc<Storage>,
    workspace: &std::path::Path,
    worker_id: &str,
    provider: Option<String>,
    command_tx: Option<mpsc::Sender<FailoverCommand>>,
    push: Option<Arc<RwLock<PushRegistry>>>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = storage
        .load_session()?
        .ok_or("no active session; run `kingdom up` first")?;

    let worker = session
        .workers
        .get(worker_id)
        .cloned()
        .ok_or_else(|| format!("worker not found: {worker_id}"))?;
    if worker.role == WorkerRole::Manager {
        return Err("cannot swap manager via `kingdom swap`".into());
    }

    ensure_swap_checkpoint(storage, workspace, &worker, push.as_ref()).await?;
    session = storage
        .load_session()?
        .ok_or("no active session; run `kingdom up` first")?;
    let worker = session
        .workers
        .get(worker_id)
        .cloned()
        .ok_or_else(|| format!("worker not found: {worker_id}"))?;

    let handoff = build_handoff_brief(&session, &worker, storage, workspace)
        .await
        .ok_or("worker has no active job to hand off")?;
    let manager_provider = session
        .manager_id
        .as_ref()
        .and_then(|id| session.workers.get(id))
        .map(|worker| worker.provider.as_str())
        .unwrap_or("n/a");
    let recommended = recommend_provider(
        &worker.provider,
        &session.available_providers,
        &FailoverReason::Manual,
        &[],
        manager_provider,
        &session,
    );

    let chosen_provider = match provider {
        Some(provider) => {
            if !session.available_providers.iter().any(|p| p == &provider) {
                return Err(format!("provider not available: {provider}").into());
            }
            if provider == worker.provider {
                return Err("target provider must differ from current worker provider".into());
            }
            provider
        }
        None => recommended
            .clone()
            .ok_or("no replacement provider available")?,
    };

    session.pending_failovers.insert(
        worker_id.to_string(),
        PendingFailover {
            worker_id: worker_id.to_string(),
            job_id: handoff.job_id.clone(),
            reason: FailoverReason::Manual,
            handoff_brief: handoff,
            recommended_provider: Some(chosen_provider.clone()),
            created_at: Utc::now(),
            status: PendingFailoverStatus::Confirmed {
                new_provider: chosen_provider.clone(),
            },
        },
    );
    storage.save_session(&session)?;
    if let Some(command_tx) = command_tx {
        command_tx
            .send(FailoverCommand::Confirm {
                worker_id: worker_id.to_string(),
                new_provider: chosen_provider.clone(),
            })
            .await
            .map_err(|_| "failover machine unavailable")?;
    }
    storage.append_action_log(&ActionLogEntry {
        timestamp: Utc::now(),
        actor: "kingdom-cli".to_string(),
        action: "swap".to_string(),
        params: serde_json::json!({
            "worker_id": worker_id,
            "new_provider": chosen_provider,
        }),
        result: None,
        error: None,
    })?;
    Ok(chosen_provider)
}

async fn ensure_swap_checkpoint(
    storage: &Arc<Storage>,
    workspace: &std::path::Path,
    worker: &crate::types::Worker,
    push: Option<&Arc<RwLock<PushRegistry>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(job_id) = worker.job_id.as_ref() else {
        return Ok(());
    };
    let mut session = storage
        .load_session()?
        .ok_or("no active session; run `kingdom up` first")?;
    let checkpoint_count = session
        .jobs
        .get(job_id)
        .map(|job| job.checkpoints.len())
        .unwrap_or(0);
    let config = crate::config::KingdomConfig::load_or_default(&storage.root.join("config.toml"));

    if let Some(push) = push {
        let notification = serde_json::json!({
            "method": "kingdom.checkpoint_request",
            "params": {
                "job_id": job_id,
                "urgency": "Critical",
            }
        });
        let _ = push.read().await.push(&worker.id, notification).await;

        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(config.failover.swap_checkpoint_timeout_seconds);
        loop {
            session = storage
                .load_session()?
                .ok_or("no active session; run `kingdom up` first")?;
            let new_count = session
                .jobs
                .get(job_id)
                .map(|job| job.checkpoints.len())
                .unwrap_or(0);
            if new_count > checkpoint_count {
                return Ok(());
            }
            if tokio::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    let content =
        crate::health::fallback_checkpoint::generate_fallback_checkpoint(job_id, workspace).await;
    storage.save_checkpoint(&content)?;
    if let Some(job) = session.jobs.get_mut(job_id) {
        job.checkpoints.push(CheckpointMeta {
            id: content.id.clone(),
            job_id: job_id.clone(),
            created_at: content.created_at,
            git_commit: content.git_commit.clone(),
        });
    }
    storage.save_session(&session)?;
    Ok(())
}

async fn try_swap_via_daemon(
    workspace: &std::path::Path,
    worker_id: &str,
    provider: Option<String>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let request = serde_json::json!({
        "cmd": "swap",
        "worker_id": worker_id,
        "provider": provider,
    });
    let socket_path = socket_path(workspace);
    match send_cli_command(&socket_path, request).await {
        Ok(_) => Ok(true),
        Err(error) if error.to_string() == "Kingdom daemon 未运行" => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CheckpointContent, CheckpointMeta, GitStrategy, Job, JobStatus, NotificationMode, Session,
        Worker, WorkerStatus,
    };
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn ts() -> chrono::DateTime<Utc> {
        Utc::now()
    }

    fn session(workspace: &std::path::Path) -> Session {
        Session {
            id: "sess_1".to_string(),
            workspace_path: workspace.display().to_string(),
            workspace_hash: "abc".to_string(),
            manager_id: Some("wm".to_string()),
            workers: [
                (
                    "wm".to_string(),
                    Worker {
                        id: "wm".to_string(),
                        provider: "claude".to_string(),
                        role: WorkerRole::Manager,
                        status: WorkerStatus::Idle,
                        job_id: None,
                        pid: None,
                        pane_id: "%0".to_string(),
                        mcp_connected: true,
                        context_usage_pct: None,
                        token_count: None,
                        last_heartbeat: None,
                        last_progress: None,
                        permissions: vec![],
                        started_at: ts(),
                    },
                ),
                (
                    "w1".to_string(),
                    Worker {
                        id: "w1".to_string(),
                        provider: "codex".to_string(),
                        role: WorkerRole::Worker,
                        status: WorkerStatus::Running,
                        job_id: Some("job_001".to_string()),
                        pid: None,
                        pane_id: "%1".to_string(),
                        mcp_connected: true,
                        context_usage_pct: None,
                        token_count: None,
                        last_heartbeat: None,
                        last_progress: None,
                        permissions: vec![],
                        started_at: ts(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
            jobs: [(
                "job_001".to_string(),
                Job {
                    id: "job_001".to_string(),
                    intent: "swap me".to_string(),
                    status: JobStatus::Running,
                    worker_id: Some("w1".to_string()),
                    depends_on: vec![],
                    created_at: ts(),
                    updated_at: ts(),
                    branch: None,
                    branch_start_commit: None,
                    checkpoints: vec![CheckpointMeta {
                        id: "ckpt_1".to_string(),
                        job_id: "job_001".to_string(),
                        created_at: ts(),
                        git_commit: None,
                    }],
                    result: None,
                    fail_count: 0,
                    last_fail_at: None,
                },
            )]
            .into_iter()
            .collect(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: GitStrategy::None,
            available_providers: vec![
                "claude".to_string(),
                "codex".to_string(),
                "gemini".to_string(),
            ],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: ts(),
        }
    }

    #[tokio::test]
    async fn run_swap_creates_confirmed_pending_failover() {
        let temp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(temp.path()).unwrap());
        let session = session(temp.path());
        storage.save_session(&session).unwrap();
        storage
            .save_checkpoint(&CheckpointContent {
                id: "ckpt_1".to_string(),
                job_id: "job_001".to_string(),
                created_at: ts(),
                done: "done".to_string(),
                abandoned: "".to_string(),
                in_progress: "progress".to_string(),
                remaining: "remaining".to_string(),
                pitfalls: "pitfalls".to_string(),
                git_commit: None,
            })
            .unwrap();

        queue_manual_swap(
            &storage,
            temp.path(),
            "w1",
            Some("gemini".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

        let loaded = storage.load_session().unwrap().unwrap();
        assert!(matches!(
            loaded.pending_failovers["w1"].status,
            PendingFailoverStatus::Confirmed { .. }
        ));
    }

    #[tokio::test]
    async fn queue_manual_swap_generates_fallback_checkpoint_after_timeout() {
        let temp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(temp.path()).unwrap());
        let session = session(temp.path());
        storage.save_session(&session).unwrap();
        std::fs::write(
            storage.root.join("config.toml"),
            toml::to_string(&{
                let mut cfg = crate::config::KingdomConfig::default_config();
                cfg.failover.swap_checkpoint_timeout_seconds = 0;
                cfg
            })
            .unwrap(),
        )
        .unwrap();

        queue_manual_swap(
            &storage,
            temp.path(),
            "w1",
            Some("gemini".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

        let loaded = storage.load_session().unwrap().unwrap();
        let checkpoint_id = loaded.jobs["job_001"]
            .checkpoints
            .last()
            .unwrap()
            .id
            .clone();
        assert!(checkpoint_id.starts_with("ckpt_fallback_"));
    }

    #[test]
    fn test_swap_no_session_returns_none() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        assert_eq!(prompt_provider_selection(&storage, "w1").unwrap(), None);
    }

    #[test]
    fn test_swap_provider_list_filters_current() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let session = session(temp.path());
        storage.save_session(&session).unwrap();

        let candidates = available_swap_candidates(&session, "codex");
        assert_eq!(candidates, vec!["claude", "gemini"]);
    }
}
