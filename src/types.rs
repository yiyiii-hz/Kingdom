use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

pub type SessionId = String;
pub type JobId = String;
pub type WorkerId = String;
pub type CheckpointId = String;
pub type NoteId = String;
pub type RequestId = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub intent: String,
    pub status: JobStatus,
    pub worker_id: Option<WorkerId>,
    pub depends_on: Vec<JobId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub branch: Option<String>,
    pub branch_start_commit: Option<String>,
    pub checkpoints: Vec<CheckpointMeta>,
    pub result: Option<JobResult>,
    pub fail_count: u32,
    pub last_fail_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JobStatus {
    Pending,
    Waiting,
    Running,
    Completed,
    Failed,
    Cancelled,
    Paused,
    Cancelling,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobResult {
    pub summary: String,
    pub changed_files: Vec<String>,
    pub completed_at: DateTime<Utc>,
    pub worker_id: WorkerId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointMeta {
    pub id: CheckpointId,
    pub job_id: JobId,
    pub created_at: DateTime<Utc>,
    pub git_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointContent {
    pub id: CheckpointId,
    pub job_id: JobId,
    pub created_at: DateTime<Utc>,
    pub done: String,
    pub abandoned: String,
    pub in_progress: String,
    pub remaining: String,
    pub pitfalls: String,
    pub git_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Worker {
    pub id: WorkerId,
    pub provider: String,
    pub role: WorkerRole,
    pub status: WorkerStatus,
    pub job_id: Option<JobId>,
    pub pid: Option<u32>,
    pub pane_id: String,
    pub mcp_connected: bool,
    pub context_usage_pct: Option<f32>,
    pub token_count: Option<u64>,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub last_progress: Option<DateTime<Utc>>,
    pub permissions: Vec<Permission>,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkerRole {
    Manager,
    Worker,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkerStatus {
    Starting,
    Running,
    Idle,
    Failed,
    Terminated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Permission {
    SubtaskCreate,
    WorkerNotify,
    WorkspaceRead,
    JobReadAll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    #[serde(rename = "session_id")]
    pub id: SessionId,
    pub workspace_path: String,
    pub workspace_hash: String,
    pub manager_id: Option<WorkerId>,
    pub workers: HashMap<WorkerId, Worker>,
    pub jobs: HashMap<JobId, Job>,
    pub notes: Vec<WorkspaceNote>,
    pub worker_seq: u32,
    pub job_seq: u32,
    pub request_seq: u32,
    pub git_strategy: GitStrategy,
    pub available_providers: Vec<String>,
    pub notification_mode: NotificationMode,
    pub pending_requests: HashMap<RequestId, PendingRequest>,
    pub pending_failovers: HashMap<WorkerId, PendingFailover>,
    #[serde(default)]
    pub provider_stability: HashMap<String, ProviderStability>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceNote {
    pub id: NoteId,
    pub content: String,
    pub scope: NoteScope,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NoteScope {
    Global,
    Directory(String),
    Job(JobId),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GitStrategy {
    Branch,
    Commit,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NotificationMode {
    Push,
    Poll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProviderStability {
    pub provider: String,
    pub crash_count: u32,
    pub timeout_count: u32,
    pub last_failure_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FailoverReason {
    Network,
    ContextLimit,
    ProcessExit { exit_code: i32 },
    HeartbeatTimeout,
    RateLimit,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffBrief {
    pub job_id: JobId,
    pub original_intent: String,
    pub done: String,
    pub in_progress: String,
    pub remaining: String,
    pub pitfalls: String,
    pub possibly_incomplete_files: Vec<String>,
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingRequest {
    pub id: RequestId,
    pub job_id: JobId,
    pub worker_id: WorkerId,
    pub question: String,
    pub blocking: bool,
    pub answer: Option<String>,
    pub answered: bool,
    pub created_at: DateTime<Utc>,
    pub answered_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingFailover {
    pub worker_id: WorkerId,
    pub job_id: JobId,
    pub reason: FailoverReason,
    pub handoff_brief: HandoffBrief,
    pub recommended_provider: Option<String>,
    pub created_at: DateTime<Utc>,
    pub status: PendingFailoverStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PendingFailoverStatus {
    WaitingConfirmation,
    Confirmed { new_provider: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HealthEvent {
    HeartbeatMissed {
        worker_id: WorkerId,
        consecutive_count: u32,
    },
    ProcessExited {
        worker_id: WorkerId,
        exit_code: i32,
    },
    ContextThreshold {
        worker_id: WorkerId,
        pct: f32,
        urgency: CheckpointUrgency,
    },
    ProgressTimeout {
        worker_id: WorkerId,
        elapsed_minutes: u32,
    },
    RateLimited {
        worker_id: WorkerId,
        retry_after_secs: u64,
        attempt: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CheckpointUrgency {
    Normal,
    High,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ManagerNotification {
    JobCompleted {
        job_id: JobId,
        worker_id: WorkerId,
        summary: String,
        changed_files: Vec<String>,
    },
    JobFailed {
        job_id: JobId,
        worker_id: WorkerId,
        reason: String,
    },
    WorkerRequest {
        job_id: JobId,
        request_id: RequestId,
        question: String,
        blocking: bool,
    },
    JobUnblocked {
        job_id: JobId,
    },
    FailoverReady {
        worker_id: WorkerId,
        reason: FailoverReason,
        candidates: Vec<String>,
    },
    WorkerIdle {
        worker_id: WorkerId,
    },
    WorkerReady {
        worker_id: WorkerId,
        provider: String,
    },
    SubtaskCreated {
        parent_job_id: JobId,
        subtask_job_id: JobId,
        intent: String,
    },
    CancelCascade {
        cancelled_job_id: JobId,
        affected_jobs: Vec<JobId>,
    },
    ProgressWarning {
        worker_id: WorkerId,
        job_id: JobId,
        elapsed_minutes: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionLogEntry {
    pub timestamp: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub params: Value,
    pub result: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RecentCalls {
    #[serde(skip, default)]
    pub cache: HashMap<(WorkerId, String), Instant>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceStatus {
    pub session_id: SessionId,
    pub manager: Option<WorkerSummary>,
    pub workers: Vec<WorkerSummary>,
    pub jobs: Vec<JobSummary>,
    pub notes: Vec<WorkspaceNote>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerSummary {
    pub id: WorkerId,
    pub provider: String,
    pub status: WorkerStatus,
    pub job_id: Option<JobId>,
    pub context_pct: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: JobId,
    pub intent: String,
    pub status: JobStatus,
    pub worker_id: Option<WorkerId>,
    pub depends_on: Vec<JobId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobStatusResponse {
    pub id: JobId,
    pub status: JobStatus,
    pub worker_id: Option<WorkerId>,
    pub checkpoint_count: u32,
    pub last_progress: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobResultResponse {
    pub id: JobId,
    pub summary: String,
    pub changed_files: Vec<String>,
    pub checkpoint_count: u32,
    pub branch: Option<String>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestStatus {
    pub request_id: RequestId,
    pub answered: bool,
    pub answer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointSummary {
    pub done: String,
    pub abandoned: String,
    pub in_progress: String,
    pub remaining: String,
    pub pitfalls: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitLogEntry {
    pub hash: String,
    pub message: String,
    pub author: String,
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde::de::DeserializeOwned;

    fn ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 5, 12, 0, 0).unwrap()
    }

    fn round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let decoded: T = serde_json::from_str(&json).unwrap();
        assert_eq!(*value, decoded);
    }

    fn sample_handoff() -> HandoffBrief {
        HandoffBrief {
            job_id: "job_001".to_string(),
            original_intent: "Implement storage".to_string(),
            done: "Defined file layout".to_string(),
            in_progress: "Writing checkpoint persistence".to_string(),
            remaining: "Add tests".to_string(),
            pitfalls: "Avoid meta.json dual writes".to_string(),
            possibly_incomplete_files: vec!["src/storage.rs".to_string()],
            changed_files: vec!["src/types.rs".to_string(), "src/storage.rs".to_string()],
        }
    }

    fn sample_job_result() -> JobResult {
        JobResult {
            summary: "M1 implemented".to_string(),
            changed_files: vec!["src/types.rs".to_string()],
            completed_at: ts(),
            worker_id: "w1".to_string(),
        }
    }

    fn sample_checkpoint_meta() -> CheckpointMeta {
        CheckpointMeta {
            id: "ckpt_20260405T120000".to_string(),
            job_id: "job_001".to_string(),
            created_at: ts(),
            git_commit: Some("abc123".to_string()),
        }
    }

    fn sample_job() -> Job {
        Job {
            id: "job_001".to_string(),
            intent: "Build M1".to_string(),
            status: JobStatus::Running,
            worker_id: Some("w1".to_string()),
            depends_on: vec!["job_000".to_string()],
            created_at: ts(),
            updated_at: ts(),
            branch: Some("kingdom/job_001".to_string()),
            branch_start_commit: Some("deadbeef".to_string()),
            checkpoints: vec![sample_checkpoint_meta()],
            result: Some(sample_job_result()),
            fail_count: 1,
            last_fail_at: Some(ts()),
        }
    }

    fn sample_worker() -> Worker {
        Worker {
            id: "w1".to_string(),
            provider: "codex".to_string(),
            role: WorkerRole::Worker,
            status: WorkerStatus::Idle,
            job_id: Some("job_001".to_string()),
            pid: Some(1234),
            pane_id: "%1".to_string(),
            mcp_connected: true,
            context_usage_pct: Some(0.42),
            token_count: Some(2048),
            last_heartbeat: Some(ts()),
            last_progress: Some(ts()),
            permissions: vec![Permission::WorkspaceRead, Permission::JobReadAll],
            started_at: ts(),
        }
    }

    fn sample_pending_request() -> PendingRequest {
        PendingRequest {
            id: "req_001".to_string(),
            job_id: "job_001".to_string(),
            worker_id: "w1".to_string(),
            question: "Proceed?".to_string(),
            blocking: true,
            answer: Some("yes".to_string()),
            answered: true,
            created_at: ts(),
            answered_at: Some(ts()),
        }
    }

    fn sample_pending_failover() -> PendingFailover {
        PendingFailover {
            worker_id: "w1".to_string(),
            job_id: "job_001".to_string(),
            reason: FailoverReason::ContextLimit,
            handoff_brief: sample_handoff(),
            recommended_provider: Some("gemini".to_string()),
            created_at: ts(),
            status: PendingFailoverStatus::Confirmed {
                new_provider: "gemini".to_string(),
            },
        }
    }

    fn sample_session() -> Session {
        let worker = sample_worker();
        let job = sample_job();
        let note = WorkspaceNote {
            id: "note_20260405T120001".to_string(),
            content: "workspace note".to_string(),
            scope: NoteScope::Directory("src".to_string()),
            created_at: ts(),
        };
        let mut workers = HashMap::new();
        workers.insert(worker.id.clone(), worker);
        let mut jobs = HashMap::new();
        jobs.insert(job.id.clone(), job);
        let mut pending_requests = HashMap::new();
        let req = sample_pending_request();
        pending_requests.insert(req.id.clone(), req);
        let mut pending_failovers = HashMap::new();
        let failover = sample_pending_failover();
        pending_failovers.insert(failover.worker_id.clone(), failover);
        let mut provider_stability = HashMap::new();
        provider_stability.insert(
            "codex".to_string(),
            ProviderStability {
                provider: "codex".to_string(),
                crash_count: 1,
                timeout_count: 0,
                last_failure_at: Some(ts()),
            },
        );

        Session {
            id: "sess_a3f9c2b1".to_string(),
            workspace_path: "/tmp/workspace".to_string(),
            workspace_hash: "a3f9c2".to_string(),
            manager_id: Some("w0".to_string()),
            workers,
            jobs,
            notes: vec![note],
            worker_seq: 2,
            job_seq: 2,
            request_seq: 2,
            git_strategy: GitStrategy::Branch,
            available_providers: vec!["claude".to_string(), "codex".to_string()],
            notification_mode: NotificationMode::Push,
            pending_requests,
            pending_failovers,
            provider_stability,
            created_at: ts(),
        }
    }

    #[test]
    fn types_round_trip() {
        round_trip(&sample_job());
        round_trip(&sample_job_result());
        round_trip(&sample_checkpoint_meta());
        round_trip(&CheckpointContent {
            id: "ckpt_20260405T120002".to_string(),
            job_id: "job_001".to_string(),
            created_at: ts(),
            done: "Done".to_string(),
            abandoned: "Abandoned".to_string(),
            in_progress: "In progress".to_string(),
            remaining: "Remaining".to_string(),
            pitfalls: "Pitfalls".to_string(),
            git_commit: Some("abc123".to_string()),
        });
        round_trip(&sample_worker());
        round_trip(&sample_session());
        round_trip(&sample_handoff());
        round_trip(&sample_pending_request());
        round_trip(&sample_pending_failover());
        round_trip(&ActionLogEntry {
            timestamp: ts(),
            actor: "w1".to_string(),
            action: "job.complete".to_string(),
            params: serde_json::json!({"job_id": "job_001"}),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        });
        round_trip(&WorkspaceStatus {
            session_id: "sess_a3f9c2b1".to_string(),
            manager: Some(WorkerSummary {
                id: "w0".to_string(),
                provider: "claude".to_string(),
                status: WorkerStatus::Idle,
                job_id: None,
                context_pct: Some(0.1),
            }),
            workers: vec![WorkerSummary {
                id: "w1".to_string(),
                provider: "codex".to_string(),
                status: WorkerStatus::Running,
                job_id: Some("job_001".to_string()),
                context_pct: Some(0.5),
            }],
            jobs: vec![JobSummary {
                id: "job_001".to_string(),
                intent: "Build M1".to_string(),
                status: JobStatus::Running,
                worker_id: Some("w1".to_string()),
                depends_on: vec![],
                created_at: ts(),
            }],
            notes: vec![],
        });
        round_trip(&JobStatusResponse {
            id: "job_001".to_string(),
            status: JobStatus::Pending,
            worker_id: Some("w1".to_string()),
            checkpoint_count: 1,
            last_progress: Some(ts()),
        });
        round_trip(&JobResultResponse {
            id: "job_001".to_string(),
            summary: "done".to_string(),
            changed_files: vec!["src/types.rs".to_string()],
            checkpoint_count: 1,
            branch: Some("kingdom/job_001".to_string()),
            completed_at: ts(),
        });
        round_trip(&RequestStatus {
            request_id: "req_001".to_string(),
            answered: true,
            answer: Some("yes".to_string()),
        });
        round_trip(&CheckpointSummary {
            done: "a".repeat(20),
            abandoned: "b".repeat(20),
            in_progress: "c".repeat(20),
            remaining: "d".repeat(20),
            pitfalls: "e".repeat(20),
        });
        round_trip(&GitLogEntry {
            hash: "abc123".to_string(),
            message: "message".to_string(),
            author: "author".to_string(),
            timestamp: ts(),
        });
    }

    #[test]
    fn enum_serialization_covers_all_m1_statuses() {
        let statuses = [
            JobStatus::Pending,
            JobStatus::Waiting,
            JobStatus::Running,
            JobStatus::Completed,
            JobStatus::Failed,
            JobStatus::Cancelled,
            JobStatus::Paused,
            JobStatus::Cancelling,
        ];
        for status in statuses {
            round_trip(&status);
        }

        for status in [
            WorkerStatus::Starting,
            WorkerStatus::Running,
            WorkerStatus::Idle,
            WorkerStatus::Failed,
            WorkerStatus::Terminated,
        ] {
            round_trip(&status);
        }

        for status in [
            PendingFailoverStatus::WaitingConfirmation,
            PendingFailoverStatus::Confirmed {
                new_provider: "codex".to_string(),
            },
            PendingFailoverStatus::Cancelled,
        ] {
            round_trip(&status);
        }
    }

    #[test]
    fn manager_notification_includes_worker_ready_variant() {
        round_trip(&ManagerNotification::WorkerReady {
            worker_id: "w1".to_string(),
            provider: "codex".to_string(),
        });
        round_trip(&ManagerNotification::CancelCascade {
            cancelled_job_id: "job_001".to_string(),
            affected_jobs: vec!["job_002".to_string(), "job_003".to_string()],
        });
        round_trip(&ManagerNotification::ProgressWarning {
            worker_id: "w1".to_string(),
            job_id: "job_001".to_string(),
            elapsed_minutes: 30,
        });
    }

    #[test]
    fn recent_calls_deserializes_as_empty_cache() {
        let value = RecentCalls::default();
        let json = serde_json::to_string(&value).unwrap();
        let decoded: RecentCalls = serde_json::from_str(&json).unwrap();
        assert!(decoded.cache.is_empty());
    }
}
