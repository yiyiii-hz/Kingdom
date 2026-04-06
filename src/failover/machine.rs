use crate::config::{FailoverConfig, HealthConfig, KingdomConfig};
use crate::failover::circuit_breaker::{CircuitBreaker, CircuitBreakerResult};
use crate::failover::handoff::{build_handoff_brief, build_manager_recovery_context};
use crate::failover::recommender::recommend_provider;
use crate::failover::stability::record_failure;
use crate::mcp::queues::NotificationQueue;
use crate::process::launcher::ProcessLauncher;
use crate::storage::Storage;
use crate::types::{
    ActionLogEntry, CheckpointUrgency, FailoverReason, HealthEvent, JobStatus, ManagerNotification,
    PendingFailover, PendingFailoverStatus, WorkerRole, WorkerStatus,
};
use chrono::Utc;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::{interval, Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum FailoverCommand {
    Confirm {
        worker_id: String,
        new_provider: String,
    },
    Cancel {
        worker_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum NormalizedFailoverEvent {
    ProcessExited {
        worker_id: String,
        reason: FailoverReason,
    },
    ManualSwap {
        worker_id: String,
        provider: Option<String>,
    },
    ContextLimit {
        worker_id: String,
        reason: FailoverReason,
    },
    HeartbeatTimeout {
        worker_id: String,
        reason: FailoverReason,
    },
    ProgressTimeout {
        worker_id: String,
        elapsed_minutes: u32,
    },
    RateLimited {
        worker_id: String,
        retry_after_secs: u64,
        attempt: u32,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct DeferredFailover {
    worker_id: String,
    reason: FailoverReason,
    ready_at: Instant,
}

pub fn normalize_event(
    event: HealthEvent,
    health: &HealthConfig,
) -> Option<NormalizedFailoverEvent> {
    match event {
        HealthEvent::ProcessExited {
            worker_id,
            exit_code,
        } => Some(NormalizedFailoverEvent::ProcessExited {
            worker_id,
            reason: FailoverReason::ProcessExit { exit_code },
        }),
        HealthEvent::HeartbeatMissed {
            worker_id,
            consecutive_count,
        } if consecutive_count >= health.heartbeat_timeout_count => {
            Some(NormalizedFailoverEvent::HeartbeatTimeout {
                worker_id,
                reason: FailoverReason::HeartbeatTimeout,
            })
        }
        HealthEvent::ContextThreshold {
            worker_id,
            urgency: CheckpointUrgency::Critical,
            ..
        } => Some(NormalizedFailoverEvent::ContextLimit {
            worker_id,
            reason: FailoverReason::ContextLimit,
        }),
        HealthEvent::ProgressTimeout {
            worker_id,
            elapsed_minutes,
        } => Some(NormalizedFailoverEvent::ProgressTimeout {
            worker_id,
            elapsed_minutes,
        }),
        HealthEvent::RateLimited {
            worker_id,
            retry_after_secs,
            attempt,
        } => Some(NormalizedFailoverEvent::RateLimited {
            worker_id,
            retry_after_secs,
            attempt,
        }),
        _ => None,
    }
}

pub fn event_priority(event: &NormalizedFailoverEvent) -> u8 {
    match event {
        NormalizedFailoverEvent::ProcessExited { .. } => 1,
        NormalizedFailoverEvent::ManualSwap { .. } => 2,
        NormalizedFailoverEvent::ContextLimit { .. } => 3,
        NormalizedFailoverEvent::HeartbeatTimeout { .. } => 4,
        NormalizedFailoverEvent::ProgressTimeout { .. } => 5,
        NormalizedFailoverEvent::RateLimited { .. } => 6,
    }
}

pub fn should_ignore_event(
    incoming: &NormalizedFailoverEvent,
    queued: &[NormalizedFailoverEvent],
    _failover: &FailoverConfig,
) -> bool {
    match incoming {
        NormalizedFailoverEvent::ProgressTimeout { worker_id, .. } => queued.iter().any(|event| {
            matches!(
                event,
                NormalizedFailoverEvent::HeartbeatTimeout { worker_id: queued_id, .. }
                    if queued_id == worker_id
            )
        }),
        _ => false,
    }
}

fn reconcile_queue(queued: &mut Vec<NormalizedFailoverEvent>, incoming: &NormalizedFailoverEvent) {
    if let NormalizedFailoverEvent::HeartbeatTimeout { worker_id, .. } = incoming {
        queued.retain(|event| {
            !matches!(
                event,
                NormalizedFailoverEvent::ProgressTimeout { worker_id: queued_id, .. }
                    if queued_id == worker_id
            )
        });
    }
}

pub struct FailoverMachine {
    storage: Arc<Storage>,
    config: Arc<RwLock<KingdomConfig>>,
    health_rx: mpsc::Receiver<HealthEvent>,
    command_rx: mpsc::Receiver<FailoverCommand>,
    notifications: Arc<Mutex<NotificationQueue>>,
    launcher: Arc<ProcessLauncher>,
    circuit_breaker: CircuitBreaker,
    session_failures: HashMap<String, Vec<String>>,
    deferred_failovers: Vec<DeferredFailover>,
}

impl FailoverMachine {
    pub fn new(
        storage: Arc<Storage>,
        config: Arc<RwLock<KingdomConfig>>,
        notifications: Arc<Mutex<NotificationQueue>>,
        health_rx: mpsc::Receiver<HealthEvent>,
        command_rx: mpsc::Receiver<FailoverCommand>,
        launcher: Arc<ProcessLauncher>,
    ) -> Self {
        let failover_config = config
            .try_read()
            .map(|guard| guard.failover.clone())
            .unwrap_or_default();
        Self {
            storage,
            config,
            health_rx,
            command_rx,
            notifications,
            launcher,
            circuit_breaker: CircuitBreaker::new(failover_config),
            session_failures: HashMap::new(),
            deferred_failovers: Vec::new(),
        }
    }

    pub async fn run(mut self) {
        let mut queued: Vec<NormalizedFailoverEvent> = Vec::new();
        let mut tick = interval(Duration::from_secs(5));
        self.process_pending_failovers().await;
        loop {
            let next_deferred_at = self
                .deferred_failovers
                .iter()
                .map(|item| item.ready_at)
                .min();
            tokio::select! {
                _ = tick.tick() => {
                    self.process_pending_failovers().await;
                }
                _ = tokio::time::sleep_until(next_deferred_at.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600))), if next_deferred_at.is_some() => {
                    self.flush_deferred_failovers().await;
                }
                maybe_command = self.command_rx.recv() => {
                    let Some(command) = maybe_command else {
                        continue;
                    };
                    self.handle_command(command).await;
                }
                maybe_event = self.health_rx.recv() => {
                    let Some(event) = maybe_event else {
                        break;
                    };
                    let config = self.config.read().await.clone();
                    self.circuit_breaker.update_config(config.failover.clone());
                    let Some(normalized) = normalize_event(event, &config.health) else {
                        continue;
                    };
                    if should_ignore_event(&normalized, &queued, &config.failover) {
                        continue;
                    }
                    reconcile_queue(&mut queued, &normalized);
                    queued.push(normalized);
                    queued.sort_by_key(event_priority);
                    while let Some(next) = queued.first().cloned() {
                        queued.remove(0);
                        self.handle_event(next, &config.failover).await;
                    }
                }
            }
        }
    }

    async fn flush_deferred_failovers(&mut self) {
        let now = Instant::now();
        let mut ready = Vec::new();
        self.deferred_failovers.retain(|item| {
            if item.ready_at <= now {
                ready.push(item.clone());
                false
            } else {
                true
            }
        });
        let config = self.config.read().await.clone();
        self.circuit_breaker.update_config(config.failover.clone());
        for deferred in ready {
            self.queue_failover(deferred.worker_id, deferred.reason, &config.failover)
                .await;
        }
    }

    async fn handle_command(&mut self, command: FailoverCommand) {
        match command {
            FailoverCommand::Confirm {
                worker_id,
                new_provider,
            } => {
                self.execute_confirmed_failover(&worker_id, &new_provider)
                    .await;
            }
            FailoverCommand::Cancel { worker_id } => {
                self.apply_cancelled_failover(&worker_id).await;
            }
        }
    }

    async fn handle_event(&mut self, event: NormalizedFailoverEvent, config: &FailoverConfig) {
        match event {
            NormalizedFailoverEvent::ProgressTimeout {
                worker_id,
                elapsed_minutes,
            } => {
                self.emit_progress_warning(&worker_id, elapsed_minutes)
                    .await;
            }
            NormalizedFailoverEvent::RateLimited {
                worker_id,
                retry_after_secs,
                attempt,
            } => {
                println!(
                    "[{worker_id}] rate limited, retrying in {retry_after_secs}s (attempt {attempt}/4)"
                );
                if attempt >= 4 {
                    self.queue_failover(worker_id, FailoverReason::Network, config)
                        .await;
                }
            }
            NormalizedFailoverEvent::ProcessExited { worker_id, reason }
            | NormalizedFailoverEvent::ContextLimit { worker_id, reason }
            | NormalizedFailoverEvent::HeartbeatTimeout { worker_id, reason } => {
                self.queue_failover(worker_id, reason, config).await;
            }
            NormalizedFailoverEvent::ManualSwap { worker_id, .. } => {
                self.queue_failover(worker_id, FailoverReason::Manual, config)
                    .await;
            }
        }
    }

    async fn emit_progress_warning(&self, worker_id: &str, elapsed_minutes: u32) {
        let Ok(Some(session)) = self.storage.load_session() else {
            return;
        };
        let Some(worker) = session.workers.get(worker_id) else {
            return;
        };
        let Some(job_id) = worker.job_id.clone() else {
            return;
        };
        self.notifications
            .lock()
            .await
            .push(ManagerNotification::ProgressWarning {
                worker_id: worker_id.to_string(),
                job_id,
                elapsed_minutes,
            });
    }

    async fn queue_failover(
        &mut self,
        worker_id: String,
        reason: FailoverReason,
        _config: &FailoverConfig,
    ) {
        let Ok(Some(mut session)) = self.storage.load_session() else {
            return;
        };
        let Some(worker) = session.workers.get(&worker_id).cloned() else {
            return;
        };

        if worker.role == WorkerRole::Worker
            && worker.status == WorkerStatus::Running
            && worker.job_id.is_some()
        {
            let job_id = worker.job_id.clone().unwrap();
            if matches!(
                session.jobs.get(&job_id).map(|job| &job.status),
                Some(JobStatus::Cancelling)
            ) {
                if let Some(job) = session.jobs.get_mut(&job_id) {
                    job.status = JobStatus::Cancelled;
                    job.updated_at = Utc::now();
                }
                if let Some(worker) = session.workers.get_mut(&worker_id) {
                    worker.status = WorkerStatus::Idle;
                    worker.job_id = None;
                }
                let _ = self.storage.save_session(&session);
                return;
            }
        }

        let handoff_brief = match worker.role {
            WorkerRole::Manager => {
                let context = build_manager_recovery_context(
                    &session,
                    &self.storage,
                    std::path::Path::new(&session.workspace_path),
                );
                crate::types::HandoffBrief {
                    job_id: "manager".to_string(),
                    original_intent: "Recover manager".to_string(),
                    done: "Manager state persisted in Kingdom".to_string(),
                    in_progress: context,
                    remaining: "Resume orchestration".to_string(),
                    pitfalls: "Do not rely on lost chat history".to_string(),
                    possibly_incomplete_files: vec![],
                    changed_files: vec![],
                }
            }
            WorkerRole::Worker => match build_handoff_brief(
                &session,
                &worker,
                &self.storage,
                std::path::Path::new(&session.workspace_path),
            )
            .await
            {
                Some(brief) => brief,
                None => return,
            },
        };

        let job_id = worker
            .job_id
            .clone()
            .unwrap_or_else(|| "manager".to_string());
        if !matches!(reason, FailoverReason::Manual) {
            match self.circuit_breaker.record_failure(&job_id, Utc::now()) {
                CircuitBreakerResult::Tripped => {
                    if let Some(job) = session.jobs.get_mut(&job_id) {
                        job.status = JobStatus::Paused;
                        job.updated_at = Utc::now();
                    }
                    self.notifications
                        .lock()
                        .await
                        .push(ManagerNotification::JobFailed {
                            job_id,
                            worker_id: worker_id.clone(),
                            reason: "circuit breaker tripped".to_string(),
                        });
                    let _ = self.storage.save_session(&session);
                    return;
                }
                CircuitBreakerResult::Ok => {}
            }

            if let Some(wait) = self.circuit_breaker.check_cooldown(&worker_id, Utc::now()) {
                self.deferred_failovers.push(DeferredFailover {
                    worker_id,
                    reason,
                    ready_at: Instant::now() + wait,
                });
                return;
            }
        }

        let session_failures = self
            .session_failures
            .get(&worker_id)
            .cloned()
            .unwrap_or_default();

        let manager_provider = session
            .manager_id
            .as_ref()
            .and_then(|id| session.workers.get(id))
            .map(|worker| worker.provider.as_str())
            .unwrap_or("n/a");
        let recommended_provider = recommend_provider(
            &worker.provider,
            &session.available_providers,
            &reason,
            &session_failures,
            manager_provider,
            &session,
        );

        session.pending_failovers.insert(
            worker_id.clone(),
            PendingFailover {
                worker_id: worker_id.clone(),
                job_id: job_id.clone(),
                reason: reason.clone(),
                handoff_brief,
                recommended_provider: recommended_provider.clone(),
                created_at: Utc::now(),
                status: PendingFailoverStatus::WaitingConfirmation,
            },
        );

        if !matches!(reason, FailoverReason::Manual) {
            self.circuit_breaker.note_failover(&worker_id, Utc::now());
            let failures = self.session_failures.entry(worker_id.clone()).or_default();
            if !failures.iter().any(|provider| provider == &worker.provider) {
                failures.push(worker.provider.clone());
            }
        }

        if let Some(worker) = session.workers.get_mut(&worker_id) {
            worker.status = WorkerStatus::Failed;
            worker.mcp_connected = false;
        }

        let _ = self.storage.save_session(&session);
        let _ = self.storage.append_action_log(&ActionLogEntry {
            timestamp: Utc::now(),
            actor: "kingdom-failover".to_string(),
            action: "failover.triggered".to_string(),
            params: serde_json::json!({
                "worker_id": worker_id,
                "job_id": job_id,
                "reason": reason,
                "recommended_provider": recommended_provider,
                "launcher_ready": Arc::strong_count(&self.launcher) >= 1,
            }),
            result: None,
            error: None,
        });

        self.notifications
            .lock()
            .await
            .push(ManagerNotification::FailoverReady {
                worker_id,
                reason,
                candidates: session.available_providers.clone(),
            });
    }

    async fn process_pending_failovers(&mut self) {
        let Ok(Some(session)) = self.storage.load_session() else {
            return;
        };
        let pending: Vec<_> = session
            .pending_failovers
            .iter()
            .map(|(worker_id, failover)| (worker_id.clone(), failover.status.clone()))
            .collect();

        for (worker_id, status) in pending {
            match status {
                PendingFailoverStatus::WaitingConfirmation => {}
                PendingFailoverStatus::Cancelled => {
                    self.apply_cancelled_failover(&worker_id).await;
                }
                PendingFailoverStatus::Confirmed { new_provider } => {
                    self.execute_confirmed_failover(&worker_id, &new_provider)
                        .await;
                }
            }
        }
    }

    async fn apply_cancelled_failover(&mut self, worker_id: &str) {
        let Ok(Some(mut session)) = self.storage.load_session() else {
            return;
        };
        let Some(pending) = session.pending_failovers.remove(worker_id) else {
            return;
        };
        if pending.job_id != "manager" {
            if let Some(job) = session.jobs.get_mut(&pending.job_id) {
                job.status = JobStatus::Paused;
                job.updated_at = Utc::now();
            }
        }
        let _ = self.storage.save_session(&session);
        self.session_failures.remove(worker_id);
        let _ = self.storage.append_action_log(&ActionLogEntry {
            timestamp: Utc::now(),
            actor: "kingdom-failover".to_string(),
            action: "failover.cancelled".to_string(),
            params: json!({ "worker_id": worker_id, "job_id": pending.job_id }),
            result: None,
            error: None,
        });
    }

    async fn execute_confirmed_failover(&mut self, worker_id: &str, new_provider: &str) {
        let Ok(Some(mut session)) = self.storage.load_session() else {
            return;
        };
        let Some(pending) = session.pending_failovers.get(worker_id).cloned() else {
            return;
        };
        let Some(worker) = session.workers.get(worker_id).cloned() else {
            return;
        };

        if let Some(pid) = worker.pid {
            let _ = self.launcher.terminate(pid, true).await;
        }

        if let Some(worker_mut) = session.workers.get_mut(worker_id) {
            worker_mut.provider = new_provider.to_string();
            worker_mut.status = WorkerStatus::Starting;
            worker_mut.mcp_connected = false;
            worker_mut.last_heartbeat = None;
            worker_mut.last_progress = None;
            worker_mut.started_at = Utc::now();
        }
        let _ = self.storage.save_session(&session);

        let worker_index = session
            .workers
            .keys()
            .position(|id| id == worker_id)
            .unwrap_or(session.workers.len());
        let launch = self
            .launcher
            .launch(
                new_provider,
                worker.role.clone(),
                worker_id,
                worker_index,
                &self.storage.root,
            )
            .await;

        let launch = match launch {
            Ok(launch) => launch,
            Err(error) => {
                self.pause_after_failed_failover(worker_id, &pending.job_id, error.to_string())
                    .await;
                return;
            }
        };

        let Ok(Some(mut session)) = self.storage.load_session() else {
            return;
        };
        if let Some(worker_mut) = session.workers.get_mut(worker_id) {
            worker_mut.pid = Some(launch.pid);
            worker_mut.pane_id = launch.pane_id.clone();
        }
        let _ = self.storage.save_session(&session);

        let timeout = self.config.read().await.failover.connect_timeout_seconds;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
        loop {
            if tokio::time::Instant::now() > deadline {
                self.pause_after_failed_failover(
                    worker_id,
                    &pending.job_id,
                    format!("replacement provider '{new_provider}' did not connect in {timeout}s"),
                )
                .await;
                return;
            }

            if let Ok(Some(mut session)) = self.storage.load_session() {
                let connected = session
                    .workers
                    .get(worker_id)
                    .map(|worker| worker.mcp_connected)
                    .unwrap_or(false);
                if connected {
                    if let Some(worker_mut) = session.workers.get_mut(worker_id) {
                        worker_mut.status = if worker_mut.job_id.is_some() {
                            WorkerStatus::Running
                        } else {
                            WorkerStatus::Idle
                        };
                    }
                    if pending.job_id != "manager" {
                        if let Some(job) = session.jobs.get_mut(&pending.job_id) {
                            job.status = JobStatus::Running;
                            job.updated_at = Utc::now();
                            job.worker_id = Some(worker_id.to_string());
                        }
                    }
                    if !matches!(pending.reason, FailoverReason::Manual) {
                        record_failure(&mut session, &worker.provider, &pending.reason, Utc::now());
                    }
                    session.pending_failovers.remove(worker_id);
                    let _ = self.storage.save_session(&session);
                    let _ = self.storage.append_action_log(&ActionLogEntry {
                        timestamp: Utc::now(),
                        actor: "kingdom-failover".to_string(),
                        action: "failover.completed".to_string(),
                        params: json!({
                            "worker_id": worker_id,
                            "job_id": pending.job_id,
                            "provider": new_provider,
                        }),
                        result: None,
                        error: None,
                    });
                    self.notifications
                        .lock()
                        .await
                        .push(ManagerNotification::WorkerReady {
                            worker_id: worker_id.to_string(),
                            provider: new_provider.to_string(),
                        });
                    return;
                }
            }

            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn pause_after_failed_failover(&self, worker_id: &str, job_id: &str, error: String) {
        let Ok(Some(mut session)) = self.storage.load_session() else {
            return;
        };
        if let Some(worker) = session.workers.get_mut(worker_id) {
            worker.status = WorkerStatus::Failed;
            worker.mcp_connected = false;
        }
        if job_id != "manager" {
            if let Some(job) = session.jobs.get_mut(job_id) {
                job.status = JobStatus::Paused;
                job.updated_at = Utc::now();
            }
        }
        session.pending_failovers.remove(worker_id);
        let _ = self.storage.save_session(&session);
        let _ = self.storage.append_action_log(&ActionLogEntry {
            timestamp: Utc::now(),
            actor: "kingdom-failover".to_string(),
            action: "failover.paused".to_string(),
            params: json!({
                "worker_id": worker_id,
                "job_id": job_id,
            }),
            result: None,
            error: Some(error.clone()),
        });
        self.notifications
            .lock()
            .await
            .push(ManagerNotification::JobFailed {
                job_id: job_id.to_string(),
                worker_id: worker_id.to_string(),
                reason: error,
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KingdomConfig;
    use crate::process::launcher::ProcessLauncher;
    use crate::storage::Storage;
    use crate::test_support::{env_lock, PathGuard};
    use crate::types::{
        GitStrategy, HandoffBrief, Job, NotificationMode, PendingFailover, Session, Worker,
        WorkerRole, WorkspaceNote,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::{mpsc, Mutex, RwLock};

    #[test]
    fn normalize_heartbeat_only_after_threshold() {
        let cfg = HealthConfig::default();
        assert_eq!(
            normalize_event(
                HealthEvent::HeartbeatMissed {
                    worker_id: "w1".to_string(),
                    consecutive_count: 1,
                },
                &cfg,
            ),
            None
        );
        assert!(matches!(
            normalize_event(
                HealthEvent::HeartbeatMissed {
                    worker_id: "w1".to_string(),
                    consecutive_count: 2,
                },
                &cfg,
            ),
            Some(NormalizedFailoverEvent::HeartbeatTimeout { .. })
        ));
    }

    #[test]
    fn normalize_context_only_for_critical_urgency() {
        let cfg = HealthConfig::default();
        assert_eq!(
            normalize_event(
                HealthEvent::ContextThreshold {
                    worker_id: "w1".to_string(),
                    pct: 0.72,
                    urgency: CheckpointUrgency::High,
                },
                &cfg,
            ),
            None
        );
        assert!(matches!(
            normalize_event(
                HealthEvent::ContextThreshold {
                    worker_id: "w1".to_string(),
                    pct: 0.91,
                    urgency: CheckpointUrgency::Critical,
                },
                &cfg,
            ),
            Some(NormalizedFailoverEvent::ContextLimit { .. })
        ));
    }

    #[test]
    fn progress_timeout_is_ignored_when_heartbeat_timeout_already_queued() {
        let ignore = should_ignore_event(
            &NormalizedFailoverEvent::ProgressTimeout {
                worker_id: "w1".to_string(),
                elapsed_minutes: 31,
            },
            &[NormalizedFailoverEvent::HeartbeatTimeout {
                worker_id: "w1".to_string(),
                reason: FailoverReason::HeartbeatTimeout,
            }],
            &FailoverConfig::default(),
        );
        assert!(ignore);
    }

    #[test]
    fn heartbeat_timeout_removes_queued_progress_timeout() {
        let mut queued = vec![NormalizedFailoverEvent::ProgressTimeout {
            worker_id: "w1".to_string(),
            elapsed_minutes: 31,
        }];
        reconcile_queue(
            &mut queued,
            &NormalizedFailoverEvent::HeartbeatTimeout {
                worker_id: "w1".to_string(),
                reason: FailoverReason::HeartbeatTimeout,
            },
        );
        assert!(queued.is_empty());
    }

    fn write_executable(path: &std::path::Path, content: &str) {
        fs::write(path, content).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn sample_worker(status: WorkerStatus) -> Worker {
        Worker {
            id: "w1".to_string(),
            provider: "codex".to_string(),
            role: WorkerRole::Worker,
            status,
            job_id: Some("job_001".to_string()),
            pid: None,
            pane_id: "%1".to_string(),
            mcp_connected: true,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: None,
            last_progress: None,
            permissions: vec![],
            started_at: Utc::now(),
        }
    }

    fn sample_job(status: JobStatus) -> Job {
        Job {
            id: "job_001".to_string(),
            intent: "Implement M7".to_string(),
            status,
            worker_id: Some("w1".to_string()),
            depends_on: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            branch: None,
            branch_start_commit: None,
            checkpoints: vec![],
            result: None,
            fail_count: 0,
            last_fail_at: None,
        }
    }

    fn sample_handoff() -> HandoffBrief {
        HandoffBrief {
            job_id: "job_001".to_string(),
            original_intent: "Implement M7".to_string(),
            done: "done".to_string(),
            in_progress: "progress".to_string(),
            remaining: "remaining".to_string(),
            pitfalls: "pitfalls".to_string(),
            possibly_incomplete_files: vec![],
            changed_files: vec![],
        }
    }

    fn sample_session(workspace: &std::path::Path, status: PendingFailoverStatus) -> Session {
        Session {
            id: "sess_test".to_string(),
            workspace_path: workspace.display().to_string(),
            workspace_hash: "abc123".to_string(),
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
                        started_at: Utc::now(),
                    },
                ),
                ("w1".to_string(), sample_worker(WorkerStatus::Failed)),
            ]
            .into_iter()
            .collect(),
            jobs: [("job_001".to_string(), sample_job(JobStatus::Running))]
                .into_iter()
                .collect(),
            notes: vec![WorkspaceNote {
                id: "note_1".to_string(),
                content: "note".to_string(),
                scope: crate::types::NoteScope::Global,
                created_at: Utc::now(),
            }],
            worker_seq: 1,
            job_seq: 1,
            request_seq: 0,
            git_strategy: GitStrategy::None,
            available_providers: vec![
                "claude".to_string(),
                "gemini".to_string(),
                "codex".to_string(),
            ],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: [(
                "w1".to_string(),
                PendingFailover {
                    worker_id: "w1".to_string(),
                    job_id: "job_001".to_string(),
                    reason: FailoverReason::ProcessExit { exit_code: 1 },
                    handoff_brief: sample_handoff(),
                    recommended_provider: Some("gemini".to_string()),
                    created_at: Utc::now(),
                    status,
                },
            )]
            .into_iter()
            .collect(),
            provider_stability: HashMap::new(),
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn confirmed_failover_launches_replacement_and_clears_pending() {
        let _env_lock = env_lock();
        let tmp = tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        write_executable(
            &bin_dir.join("tmux"),
            "#!/bin/sh\ncase \"$1\" in\n  split-window) echo %1 ;;\n  new-window) echo %2 ;;\n  display-message) echo 4242 ;;\n  send-keys) exit 0 ;;\n  *) exit 0 ;;\nesac\n",
        );
        write_executable(&bin_dir.join("gemini-provider"), "#!/bin/sh\nsleep 1\n");
        let _path_guard = PathGuard::prepend(&bin_dir);

        let storage = Arc::new(Storage::init(tmp.path()).unwrap());
        let session = sample_session(
            tmp.path(),
            PendingFailoverStatus::Confirmed {
                new_provider: "gemini".to_string(),
            },
        );
        storage.save_session(&session).unwrap();

        let mut config = KingdomConfig::default_config();
        config.providers.overrides.insert(
            "gemini".to_string(),
            bin_dir.join("gemini-provider").display().to_string(),
        );
        config.failover.connect_timeout_seconds = 1;

        let launcher = Arc::new(ProcessLauncher::new(
            tmp.path().to_path_buf(),
            config.clone(),
            "abc123".to_string(),
        ));
        let (_tx, rx) = mpsc::channel(1);
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let mut machine = FailoverMachine::new(
            Arc::clone(&storage),
            Arc::new(RwLock::new(config)),
            Arc::new(Mutex::new(crate::mcp::queues::NotificationQueue::new())),
            rx,
            cmd_rx,
            launcher,
        );

        let storage_clone = Arc::clone(&storage);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut session = storage_clone.load_session().unwrap().unwrap();
            let worker = session.workers.get_mut("w1").unwrap();
            worker.mcp_connected = true;
            storage_clone.save_session(&session).unwrap();
        });

        machine.process_pending_failovers().await;

        let session = storage.load_session().unwrap().unwrap();
        assert!(!session.pending_failovers.contains_key("w1"));
        assert_eq!(session.workers["w1"].provider, "gemini");
        assert_eq!(session.workers["w1"].status, WorkerStatus::Running);
        assert_eq!(session.jobs["job_001"].status, JobStatus::Running);
    }

    #[tokio::test]
    async fn cancelled_failover_pauses_job() {
        let tmp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(tmp.path()).unwrap());
        let session = sample_session(tmp.path(), PendingFailoverStatus::Cancelled);
        storage.save_session(&session).unwrap();

        let config = KingdomConfig::default_config();
        let launcher = Arc::new(ProcessLauncher::new(
            tmp.path().to_path_buf(),
            config.clone(),
            "abc123".to_string(),
        ));
        let (_tx, rx) = mpsc::channel(1);
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let mut machine = FailoverMachine::new(
            Arc::clone(&storage),
            Arc::new(RwLock::new(config)),
            Arc::new(Mutex::new(crate::mcp::queues::NotificationQueue::new())),
            rx,
            cmd_rx,
            launcher,
        );

        machine.process_pending_failovers().await;

        let session = storage.load_session().unwrap().unwrap();
        assert!(!session.pending_failovers.contains_key("w1"));
        assert_eq!(session.jobs["job_001"].status, JobStatus::Paused);
    }

    #[tokio::test]
    async fn failover_command_cancel_applies_without_polling() {
        let tmp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(tmp.path()).unwrap());
        let session = sample_session(tmp.path(), PendingFailoverStatus::Cancelled);
        storage.save_session(&session).unwrap();

        let config = KingdomConfig::default_config();
        let launcher = Arc::new(ProcessLauncher::new(
            tmp.path().to_path_buf(),
            config.clone(),
            "abc123".to_string(),
        ));
        let (_tx, rx) = mpsc::channel(1);
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let mut machine = FailoverMachine::new(
            Arc::clone(&storage),
            Arc::new(RwLock::new(config)),
            Arc::new(Mutex::new(crate::mcp::queues::NotificationQueue::new())),
            rx,
            cmd_rx,
            launcher,
        );

        machine
            .handle_command(FailoverCommand::Cancel {
                worker_id: "w1".to_string(),
            })
            .await;

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].status, JobStatus::Paused);
        assert!(!session.pending_failovers.contains_key("w1"));
    }

    #[tokio::test]
    async fn automatic_failover_records_stability_only_after_success() {
        let _env_lock = env_lock();
        let tmp = tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        write_executable(
            &bin_dir.join("tmux"),
            "#!/bin/sh\ncase \"$1\" in\n  split-window) echo %1 ;;\n  display-message) echo 4242 ;;\n  send-keys) exit 0 ;;\n  *) exit 0 ;;\nesac\n",
        );
        write_executable(&bin_dir.join("claude-provider"), "#!/bin/sh\nsleep 1\n");
        let _path_guard = PathGuard::prepend(&bin_dir);

        let storage = Arc::new(Storage::init(tmp.path()).unwrap());
        let session = sample_session(
            tmp.path(),
            PendingFailoverStatus::Confirmed {
                new_provider: "claude".to_string(),
            },
        );
        storage.save_session(&session).unwrap();

        let mut config = KingdomConfig::default_config();
        config.providers.overrides.insert(
            "claude".to_string(),
            bin_dir.join("claude-provider").display().to_string(),
        );
        config.failover.connect_timeout_seconds = 1;

        let launcher = Arc::new(ProcessLauncher::new(
            tmp.path().to_path_buf(),
            config.clone(),
            "abc123".to_string(),
        ));
        let (_tx, rx) = mpsc::channel(1);
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let mut machine = FailoverMachine::new(
            Arc::clone(&storage),
            Arc::new(RwLock::new(config)),
            Arc::new(Mutex::new(crate::mcp::queues::NotificationQueue::new())),
            rx,
            cmd_rx,
            launcher,
        );

        let before = storage.load_session().unwrap().unwrap();
        assert!(before.provider_stability.get("codex").is_none());

        let storage_clone = Arc::clone(&storage);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut session = storage_clone.load_session().unwrap().unwrap();
            session.workers.get_mut("w1").unwrap().mcp_connected = true;
            storage_clone.save_session(&session).unwrap();
        });

        machine
            .handle_command(FailoverCommand::Confirm {
                worker_id: "w1".to_string(),
                new_provider: "claude".to_string(),
            })
            .await;

        let session = storage.load_session().unwrap().unwrap();
        let stability = session.provider_stability.get("codex").unwrap();
        assert_eq!(stability.crash_count, 1);
    }

    #[tokio::test]
    async fn cancelling_process_exit_marks_job_cancelled_without_failover() {
        let tmp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(tmp.path()).unwrap());
        let mut session = sample_session(tmp.path(), PendingFailoverStatus::WaitingConfirmation);
        session.pending_failovers.clear();
        session.jobs.get_mut("job_001").unwrap().status = JobStatus::Cancelling;
        session.workers.get_mut("w1").unwrap().status = WorkerStatus::Running;
        storage.save_session(&session).unwrap();

        let config = KingdomConfig::default_config();
        let launcher = Arc::new(ProcessLauncher::new(
            tmp.path().to_path_buf(),
            config.clone(),
            "abc123".to_string(),
        ));
        let (_tx, rx) = mpsc::channel(1);
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let mut machine = FailoverMachine::new(
            Arc::clone(&storage),
            Arc::new(RwLock::new(config)),
            Arc::new(Mutex::new(crate::mcp::queues::NotificationQueue::new())),
            rx,
            cmd_rx,
            launcher,
        );

        machine
            .queue_failover(
                "w1".to_string(),
                FailoverReason::ProcessExit { exit_code: 1 },
                &FailoverConfig::default(),
            )
            .await;

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].status, JobStatus::Cancelled);
        assert_eq!(session.workers["w1"].status, WorkerStatus::Idle);
        assert!(session.pending_failovers.is_empty());
    }

    #[tokio::test]
    async fn manual_swap_ignores_circuit_breaker_and_cooldown() {
        let tmp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(tmp.path()).unwrap());
        let mut session = sample_session(tmp.path(), PendingFailoverStatus::WaitingConfirmation);
        session.pending_failovers.clear();
        session.manager_id = None;
        session.available_providers = vec!["codex".to_string(), "gemini".to_string()];
        storage.save_session(&session).unwrap();

        let mut config = KingdomConfig::default_config();
        config.failover.failure_threshold = 1;
        config.failover.cooldown_seconds = 30;
        let launcher = Arc::new(ProcessLauncher::new(
            tmp.path().to_path_buf(),
            config.clone(),
            "abc123".to_string(),
        ));
        let (_tx, rx) = mpsc::channel(1);
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let mut machine = FailoverMachine::new(
            Arc::clone(&storage),
            Arc::new(RwLock::new(config)),
            Arc::new(Mutex::new(crate::mcp::queues::NotificationQueue::new())),
            rx,
            cmd_rx,
            launcher,
        );
        machine.circuit_breaker.note_failover("w1", Utc::now());

        machine
            .queue_failover(
                "w1".to_string(),
                FailoverReason::Manual,
                &FailoverConfig::default(),
            )
            .await;

        let session = storage.load_session().unwrap().unwrap();
        assert!(session.pending_failovers.contains_key("w1"));
        assert!(machine.deferred_failovers.is_empty());
    }

    #[tokio::test]
    async fn auto_failover_respects_session_failure_chain_for_recommendation() {
        let tmp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(tmp.path()).unwrap());
        let mut session = sample_session(tmp.path(), PendingFailoverStatus::WaitingConfirmation);
        session.pending_failovers.clear();
        storage.save_session(&session).unwrap();

        let config = KingdomConfig::default_config();
        let launcher = Arc::new(ProcessLauncher::new(
            tmp.path().to_path_buf(),
            config.clone(),
            "abc123".to_string(),
        ));
        let (_tx, rx) = mpsc::channel(1);
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let mut machine = FailoverMachine::new(
            Arc::clone(&storage),
            Arc::new(RwLock::new(config)),
            Arc::new(Mutex::new(crate::mcp::queues::NotificationQueue::new())),
            rx,
            cmd_rx,
            launcher,
        );
        machine
            .session_failures
            .insert("w1".to_string(), vec!["gemini".to_string()]);

        machine
            .queue_failover(
                "w1".to_string(),
                FailoverReason::ProcessExit { exit_code: 1 },
                &FailoverConfig::default(),
            )
            .await;

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.pending_failovers["w1"].recommended_provider, None);
    }

    #[tokio::test]
    async fn auto_failover_during_cooldown_is_deferred_but_manual_command_still_runs() {
        let tmp = tempdir().unwrap();
        let storage = Arc::new(Storage::init(tmp.path()).unwrap());
        let mut session = sample_session(tmp.path(), PendingFailoverStatus::WaitingConfirmation);
        session.pending_failovers.clear();
        storage.save_session(&session).unwrap();

        let mut config = KingdomConfig::default_config();
        config.failover.cooldown_seconds = 30;
        let launcher = Arc::new(ProcessLauncher::new(
            tmp.path().to_path_buf(),
            config.clone(),
            "abc123".to_string(),
        ));
        let (_tx, rx) = mpsc::channel(1);
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let mut machine = FailoverMachine::new(
            Arc::clone(&storage),
            Arc::new(RwLock::new(config)),
            Arc::new(Mutex::new(crate::mcp::queues::NotificationQueue::new())),
            rx,
            cmd_rx,
            launcher,
        );
        machine.circuit_breaker.note_failover("w1", Utc::now());

        machine
            .queue_failover(
                "w1".to_string(),
                FailoverReason::HeartbeatTimeout,
                &FailoverConfig::default(),
            )
            .await;
        assert_eq!(machine.deferred_failovers.len(), 1);

        let mut session = storage.load_session().unwrap().unwrap();
        session.pending_failovers.insert(
            "w1".to_string(),
            PendingFailover {
                worker_id: "w1".to_string(),
                job_id: "job_001".to_string(),
                reason: FailoverReason::Manual,
                handoff_brief: sample_handoff(),
                recommended_provider: Some("claude".to_string()),
                created_at: Utc::now(),
                status: PendingFailoverStatus::Cancelled,
            },
        );
        storage.save_session(&session).unwrap();
        machine
            .handle_command(FailoverCommand::Cancel {
                worker_id: "w1".to_string(),
            })
            .await;

        let session = storage.load_session().unwrap().unwrap();
        assert_eq!(session.jobs["job_001"].status, JobStatus::Paused);
    }
}
