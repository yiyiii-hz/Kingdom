use crate::config::HealthConfig;
use crate::mcp::push::PushRegistry;
use crate::mcp::queues::HealthEventQueue;
use crate::storage::Storage;
use crate::types::{
    ActionLogEntry, CheckpointMeta, CheckpointUrgency, HealthEvent, Session, WorkerStatus,
};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};

#[derive(Debug, Clone)]
pub struct PendingCheckpoint {
    pub job_id: String,
    pub urgency: CheckpointUrgency,
    pub sent_at: DateTime<Utc>,
    pub checkpoint_count_at_send: usize,
}

pub struct HealthMonitor {
    session: Arc<Mutex<Session>>,
    config: HealthConfig,
    event_tx: mpsc::Sender<HealthEvent>,
    push: Arc<RwLock<PushRegistry>>,
    health_event_queue: Arc<Mutex<HealthEventQueue>>,
    storage: Arc<Storage>,
}

impl HealthMonitor {
    pub fn new(
        session: Arc<Mutex<Session>>,
        config: HealthConfig,
        event_tx: mpsc::Sender<HealthEvent>,
        push: Arc<RwLock<PushRegistry>>,
        health_event_queue: Arc<Mutex<HealthEventQueue>>,
        storage: Arc<Storage>,
    ) -> Self {
        Self {
            session,
            config,
            event_tx,
            push,
            health_event_queue,
            storage,
        }
    }

    pub async fn run(&self) {
        tokio::join!(
            self.heartbeat_loop(),
            self.process_loop(),
            self.context_loop(),
            self.progress_loop(),
        );
    }

    async fn heartbeat_loop(&self) {
        let mut miss_counts: HashMap<String, u32> = HashMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(
                self.config.heartbeat_interval_seconds,
            ))
            .await;
            let session = self.session.lock().await;
            let events = check_heartbeats(&session, &self.config, Utc::now(), &mut miss_counts);
            drop(session);
            for event in events {
                self.log_and_send(&event).await;
                let _ = self.event_tx.send(event).await;
            }
        }
    }

    async fn process_loop(&self) {
        let mut already_reported: HashSet<String> = HashSet::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(
                self.config.process_check_interval_seconds,
            ))
            .await;
            let pairs = {
                let session = self.session.lock().await;
                for (worker_id, worker) in &session.workers {
                    if worker.status == WorkerStatus::Terminated {
                        already_reported.remove(worker_id);
                    }
                }
                list_trackable_pids(&session)
            };
            for (worker_id, pid) in pairs {
                if already_reported.contains(&worker_id) {
                    continue;
                }
                if !is_process_alive(pid) {
                    already_reported.insert(worker_id.clone());
                    let exit_code = try_waitpid(pid).unwrap_or(-1);
                    let event = HealthEvent::ProcessExited {
                        worker_id,
                        exit_code,
                    };
                    self.log_and_send(&event).await;
                    let _ = self.event_tx.send(event).await;
                }
            }
        }
    }

    async fn context_loop(&self) {
        let mut pending: HashMap<String, PendingCheckpoint> = HashMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(
                self.config.heartbeat_interval_seconds,
            ))
            .await;
            let now = Utc::now();

            let context_events = self.health_event_queue.lock().await.drain();
            for event in context_events {
                if let HealthEvent::ContextThreshold {
                    ref worker_id,
                    ref urgency,
                    ..
                } = event
                {
                    if should_send_checkpoint_request(&pending, worker_id) {
                        let session = self.session.lock().await;
                        if let Some(worker) = session.workers.get(worker_id) {
                            if let Some(job_id) = &worker.job_id {
                                let checkpoint_count = session
                                    .jobs
                                    .get(job_id)
                                    .map(|job| job.checkpoints.len())
                                    .unwrap_or(0);
                                let urgency_str = match urgency {
                                    CheckpointUrgency::Normal => "Normal",
                                    CheckpointUrgency::High => "High",
                                    CheckpointUrgency::Critical => "Critical",
                                };
                                let notification = serde_json::json!({
                                    "method": "kingdom.checkpoint_request",
                                    "params": {
                                        "job_id": job_id,
                                        "urgency": urgency_str,
                                    }
                                });
                                let push = self.push.read().await;
                                let _ = push.push(worker_id, notification).await;
                                drop(push);

                                pending.insert(
                                    worker_id.clone(),
                                    PendingCheckpoint {
                                        job_id: job_id.clone(),
                                        urgency: urgency.clone(),
                                        sent_at: now,
                                        checkpoint_count_at_send: checkpoint_count,
                                    },
                                );
                            }
                        }
                    }
                }
            }

            let mut to_remove = Vec::new();
            let mut fallbacks = Vec::new();
            {
                let session = self.session.lock().await;
                for (worker_id, p) in &pending {
                    if checkpoint_was_answered(&session, p) {
                        to_remove.push(worker_id.clone());
                    } else if checkpoint_timed_out(p, now) {
                        fallbacks.push((worker_id.clone(), p.job_id.clone()));
                        to_remove.push(worker_id.clone());
                    }
                }
            }
            for worker_id in to_remove {
                pending.remove(&worker_id);
            }

            let workspace_path = {
                let session = self.session.lock().await;
                session.workspace_path.clone()
            };
            for (_worker_id, job_id) in fallbacks {
                let content = crate::health::fallback_checkpoint::generate_fallback_checkpoint(
                    &job_id,
                    Path::new(&workspace_path),
                )
                .await;
                let _ = self.storage.save_checkpoint(&content);
                let mut session = self.session.lock().await;
                if let Some(job) = session.jobs.get_mut(&job_id) {
                    job.checkpoints.push(CheckpointMeta {
                        id: content.id.clone(),
                        job_id: job_id.clone(),
                        created_at: content.created_at,
                        git_commit: content.git_commit.clone(),
                    });
                }
                let _ = self.storage.save_session(&session);
            }
        }
    }

    async fn progress_loop(&self) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let session = self.session.lock().await;
            let events = check_progress_timeouts(&session, &self.config, Utc::now());
            drop(session);
            for event in events {
                self.log_and_send(&event).await;
                let _ = self.event_tx.send(event).await;
            }
        }
    }

    async fn log_and_send(&self, event: &HealthEvent) {
        let entry = ActionLogEntry {
            timestamp: Utc::now(),
            actor: "kingdom-health".to_string(),
            action: "health_event".to_string(),
            params: serde_json::to_value(event).unwrap_or_default(),
            result: None,
            error: None,
        };
        let _ = self.storage.append_action_log(&entry);
    }
}

pub fn check_heartbeats(
    session: &Session,
    config: &HealthConfig,
    now: DateTime<Utc>,
    miss_counts: &mut HashMap<String, u32>,
) -> Vec<HealthEvent> {
    let threshold = chrono::Duration::seconds(
        (config.heartbeat_interval_seconds * u64::from(config.heartbeat_timeout_count)) as i64,
    );
    let mut events = Vec::new();

    for (worker_id, worker) in &session.workers {
        if !worker.mcp_connected || worker.status != WorkerStatus::Running {
            miss_counts.remove(worker_id);
            continue;
        }
        let last = worker.last_heartbeat.unwrap_or(worker.started_at);
        let elapsed = now - last;
        if elapsed >= threshold {
            let count = miss_counts.entry(worker_id.clone()).or_insert(0);
            *count += 1;
            events.push(HealthEvent::HeartbeatMissed {
                worker_id: worker_id.clone(),
                consecutive_count: *count,
            });
        } else {
            miss_counts.remove(worker_id);
        }
    }
    events
}

pub fn list_trackable_pids(session: &Session) -> Vec<(String, u32)> {
    session
        .workers
        .values()
        .filter_map(|w| {
            if w.status == WorkerStatus::Terminated {
                return None;
            }
            w.pid.map(|pid| (w.id.clone(), pid))
        })
        .collect()
}

pub fn is_process_alive(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

pub fn check_progress_timeouts(
    session: &Session,
    config: &HealthConfig,
    now: DateTime<Utc>,
) -> Vec<HealthEvent> {
    let timeout = chrono::Duration::minutes(config.progress_timeout_minutes as i64);
    session
        .workers
        .values()
        .filter_map(|w| {
            if w.status != WorkerStatus::Running {
                return None;
            }
            let last = w.last_progress.unwrap_or(w.started_at);
            let elapsed = now - last;
            if elapsed >= timeout {
                let elapsed_minutes = elapsed.num_minutes() as u32;
                Some(HealthEvent::ProgressTimeout {
                    worker_id: w.id.clone(),
                    elapsed_minutes,
                })
            } else {
                None
            }
        })
        .collect()
}

pub fn should_send_checkpoint_request(
    pending: &HashMap<String, PendingCheckpoint>,
    worker_id: &str,
) -> bool {
    !pending.contains_key(worker_id)
}

pub fn checkpoint_was_answered(session: &Session, pending: &PendingCheckpoint) -> bool {
    session
        .jobs
        .get(&pending.job_id)
        .map(|job| job.checkpoints.len() > pending.checkpoint_count_at_send)
        .unwrap_or(false)
}

pub fn checkpoint_timed_out(pending: &PendingCheckpoint, now: DateTime<Utc>) -> bool {
    let window = match pending.urgency {
        CheckpointUrgency::Normal => chrono::Duration::seconds(60),
        CheckpointUrgency::High => chrono::Duration::seconds(15),
        CheckpointUrgency::Critical => chrono::Duration::seconds(0),
    };
    now - pending.sent_at >= window
}

fn try_waitpid(pid: u32) -> Option<i32> {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;

    match waitpid(Pid::from_raw(pid as i32), Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::Exited(_, code)) => Some(code),
        Ok(WaitStatus::Signaled(_, sig, _)) => Some(-(sig as i32)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HealthConfig;
    use crate::types::{Job, JobStatus, NotificationMode, Worker, WorkerRole};
    use std::collections::HashMap;

    fn cfg() -> HealthConfig {
        HealthConfig::default()
    }

    fn make_session(workers: Vec<Worker>, jobs: Vec<Job>) -> Session {
        Session {
            id: "sess_test".to_string(),
            workspace_path: "/tmp".to_string(),
            workspace_hash: "abc123".to_string(),
            manager_id: None,
            workers: workers.into_iter().map(|w| (w.id.clone(), w)).collect(),
            jobs: jobs.into_iter().map(|j| (j.id.clone(), j)).collect(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: crate::types::GitStrategy::None,
            available_providers: vec![],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            created_at: Utc::now(),
        }
    }

    fn running_worker(id: &str, started_minutes_ago: i64) -> Worker {
        Worker {
            id: id.to_string(),
            provider: "codex".to_string(),
            role: WorkerRole::Worker,
            status: WorkerStatus::Running,
            job_id: None,
            pid: Some(99999),
            pane_id: String::new(),
            mcp_connected: true,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: Some(Utc::now() - chrono::Duration::minutes(started_minutes_ago)),
            last_progress: Some(Utc::now() - chrono::Duration::minutes(started_minutes_ago)),
            permissions: vec![],
            started_at: Utc::now() - chrono::Duration::minutes(started_minutes_ago),
        }
    }

    #[test]
    fn heartbeat_triggers_after_timeout() {
        let cfg = HealthConfig {
            heartbeat_interval_seconds: 30,
            heartbeat_timeout_count: 2,
            ..cfg()
        };
        let mut w = running_worker("w1", 0);
        w.last_heartbeat = Some(Utc::now() - chrono::Duration::seconds(61));
        let session = make_session(vec![w], vec![]);
        let mut miss_counts = HashMap::new();
        let events = check_heartbeats(&session, &cfg, Utc::now(), &mut miss_counts);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            HealthEvent::HeartbeatMissed {
                consecutive_count: 1,
                ..
            }
        ));
    }

    #[test]
    fn heartbeat_no_trigger_before_timeout() {
        let cfg = HealthConfig {
            heartbeat_interval_seconds: 30,
            heartbeat_timeout_count: 2,
            ..cfg()
        };
        let mut w = running_worker("w1", 0);
        w.last_heartbeat = Some(Utc::now() - chrono::Duration::seconds(30));
        let session = make_session(vec![w], vec![]);
        let mut miss_counts = HashMap::new();
        let events = check_heartbeats(&session, &cfg, Utc::now(), &mut miss_counts);
        assert!(events.is_empty());
    }

    #[test]
    fn heartbeat_consecutive_count_increments() {
        let cfg = HealthConfig {
            heartbeat_interval_seconds: 30,
            heartbeat_timeout_count: 2,
            ..cfg()
        };
        let mut w = running_worker("w1", 0);
        w.last_heartbeat = Some(Utc::now() - chrono::Duration::seconds(61));
        let session = make_session(vec![w], vec![]);
        let mut miss_counts = HashMap::new();
        check_heartbeats(&session, &cfg, Utc::now(), &mut miss_counts);
        let events = check_heartbeats(&session, &cfg, Utc::now(), &mut miss_counts);
        assert!(matches!(
            events[0],
            HealthEvent::HeartbeatMissed {
                consecutive_count: 2,
                ..
            }
        ));
    }

    #[test]
    fn heartbeat_skips_disconnected_workers() {
        let cfg = cfg();
        let mut w = running_worker("w1", 5);
        w.mcp_connected = false;
        w.last_heartbeat = Some(Utc::now() - chrono::Duration::seconds(300));
        let session = make_session(vec![w], vec![]);
        let mut miss_counts = HashMap::new();
        let events = check_heartbeats(&session, &cfg, Utc::now(), &mut miss_counts);
        assert!(events.is_empty());
    }

    #[test]
    fn progress_timeout_triggers() {
        let cfg = HealthConfig {
            progress_timeout_minutes: 30,
            ..cfg()
        };
        let mut w = running_worker("w1", 31);
        w.last_progress = Some(Utc::now() - chrono::Duration::minutes(31));
        let session = make_session(vec![w], vec![]);
        let events = check_progress_timeouts(&session, &cfg, Utc::now());
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            HealthEvent::ProgressTimeout {
                elapsed_minutes, ..
            } if elapsed_minutes >= 31
        ));
    }

    #[test]
    fn progress_timeout_skips_idle_workers() {
        let cfg = HealthConfig {
            progress_timeout_minutes: 5,
            ..cfg()
        };
        let mut w = running_worker("w1", 60);
        w.status = WorkerStatus::Idle;
        let session = make_session(vec![w], vec![]);
        let events = check_progress_timeouts(&session, &cfg, Utc::now());
        assert!(events.is_empty());
    }

    #[test]
    fn should_send_when_no_pending() {
        let pending = HashMap::new();
        assert!(should_send_checkpoint_request(&pending, "w1"));
    }

    #[test]
    fn should_not_send_when_already_pending() {
        let mut pending = HashMap::new();
        pending.insert(
            "w1".to_string(),
            PendingCheckpoint {
                job_id: "job_001".to_string(),
                urgency: CheckpointUrgency::Normal,
                sent_at: Utc::now(),
                checkpoint_count_at_send: 0,
            },
        );
        assert!(!should_send_checkpoint_request(&pending, "w1"));
    }

    #[test]
    fn checkpoint_answered_when_new_checkpoint_added() {
        let job = Job {
            id: "job_001".to_string(),
            intent: "test".to_string(),
            status: JobStatus::Running,
            worker_id: Some("w1".to_string()),
            depends_on: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            branch: None,
            branch_start_commit: None,
            checkpoints: vec![CheckpointMeta {
                id: "ckpt_001".to_string(),
                job_id: "job_001".to_string(),
                created_at: Utc::now(),
                git_commit: None,
            }],
            result: None,
            fail_count: 0,
            last_fail_at: None,
        };
        let session = make_session(vec![], vec![job]);
        let pending = PendingCheckpoint {
            job_id: "job_001".to_string(),
            urgency: CheckpointUrgency::Normal,
            sent_at: Utc::now(),
            checkpoint_count_at_send: 0,
        };
        assert!(checkpoint_was_answered(&session, &pending));
    }

    #[test]
    fn checkpoint_timed_out_normal() {
        let pending = PendingCheckpoint {
            job_id: "job_001".to_string(),
            urgency: CheckpointUrgency::Normal,
            sent_at: Utc::now() - chrono::Duration::seconds(61),
            checkpoint_count_at_send: 0,
        };
        assert!(checkpoint_timed_out(&pending, Utc::now()));
    }

    #[test]
    fn checkpoint_not_timed_out_normal_within_window() {
        let pending = PendingCheckpoint {
            job_id: "job_001".to_string(),
            urgency: CheckpointUrgency::Normal,
            sent_at: Utc::now() - chrono::Duration::seconds(30),
            checkpoint_count_at_send: 0,
        };
        assert!(!checkpoint_timed_out(&pending, Utc::now()));
    }

    #[test]
    fn checkpoint_timed_out_critical_immediately() {
        let pending = PendingCheckpoint {
            job_id: "job_001".to_string(),
            urgency: CheckpointUrgency::Critical,
            sent_at: Utc::now() - chrono::Duration::seconds(1),
            checkpoint_count_at_send: 0,
        };
        assert!(checkpoint_timed_out(&pending, Utc::now()));
    }
}
