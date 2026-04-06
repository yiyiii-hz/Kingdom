use crate::types::{ActionLogEntry, CheckpointContent, HandoffBrief, Job, JobResult, Session};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const KINGDOM_DIR: &str = ".kingdom";
const STATE_FILE: &str = "state.json";
const LOGS_DIR: &str = "logs";
const ACTION_LOG_FILE: &str = "action.jsonl";
const JOBS_DIR: &str = "jobs";
const CHECKPOINTS_DIR: &str = "checkpoints";
const HANDOFF_FILE: &str = "handoff.md";
const RESULT_FILE: &str = "result.json";
const ARCHIVE_DIR: &str = "archive";
const GITIGNORE_FILE: &str = ".gitignore";

pub type Result<T> = std::result::Result<T, StorageError>;

#[derive(Debug)]
pub enum StorageError {
    Io(io::Error),
    Json(serde_json::Error),
    NoSession,
}

impl Display for StorageError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::NoSession => write!(f, "no active session: call save_session before save_job"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<io::Error> for StorageError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for StorageError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Storage {
    pub root: PathBuf,
}

impl Storage {
    pub fn init(workspace: &Path) -> Result<Self> {
        let root = workspace.join(KINGDOM_DIR);
        fs::create_dir_all(root.join(LOGS_DIR))?;
        fs::create_dir_all(root.join(JOBS_DIR))?;

        let gitignore_path = root.join(GITIGNORE_FILE);
        if !gitignore_path.exists() {
            fs::write(&gitignore_path, "*\n")?;
        }

        let action_log_path = root.join(LOGS_DIR).join(ACTION_LOG_FILE);
        if !action_log_path.exists() {
            File::create(action_log_path)?;
        }

        Ok(Self { root })
    }

    pub fn load_session(&self) -> Result<Option<Session>> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(None);
        }
        let session = serde_json::from_slice(&fs::read(path)?)?;
        Ok(Some(session))
    }

    pub fn save_session(&self, session: &Session) -> Result<()> {
        self.write_json_atomic(&self.state_path(), session)
    }

    pub fn load_job(&self, job_id: &str) -> Result<Option<Job>> {
        Ok(self
            .load_session()?
            .and_then(|session| session.jobs.get(job_id).cloned()))
    }

    pub fn save_job(&self, job: &Job) -> Result<()> {
        let mut session = self.load_session()?.ok_or(StorageError::NoSession)?;
        session.jobs.insert(job.id.clone(), job.clone());
        self.save_session(&session)
    }

    pub fn load_checkpoint(&self, job_id: &str, checkpoint_id: &str) -> Result<CheckpointContent> {
        let path = self.checkpoint_path(job_id, checkpoint_id);
        let content = serde_json::from_slice(&fs::read(path)?)?;
        Ok(content)
    }

    pub fn save_checkpoint(&self, content: &CheckpointContent) -> Result<()> {
        let checkpoints_dir = self.job_dir(&content.job_id).join(CHECKPOINTS_DIR);
        fs::create_dir_all(checkpoints_dir.as_path())?;
        self.write_json_atomic(&self.checkpoint_path(&content.job_id, &content.id), content)
    }

    pub fn save_handoff(&self, job_id: &str, brief: &HandoffBrief) -> Result<()> {
        let job_dir = self.job_dir(job_id);
        fs::create_dir_all(job_dir.as_path())?;
        fs::write(job_dir.join(HANDOFF_FILE), render_handoff_markdown(brief))?;
        Ok(())
    }

    pub fn save_result(&self, job_id: &str, result: &JobResult) -> Result<()> {
        let job_dir = self.job_dir(job_id);
        fs::create_dir_all(job_dir.as_path())?;
        self.write_json_atomic(&job_dir.join(RESULT_FILE), result)
    }

    pub fn append_action_log(&self, entry: &ActionLogEntry) -> Result<()> {
        let path = self.root.join(LOGS_DIR).join(ACTION_LOG_FILE);
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        serde_json::to_writer(&mut file, entry)?;
        writeln!(file)?;
        Ok(())
    }

    pub fn read_action_log(&self, limit: Option<usize>) -> Result<Vec<ActionLogEntry>> {
        let path = self.root.join(LOGS_DIR).join(ACTION_LOG_FILE);
        if !path.exists() {
            return Ok(Vec::new());
        }

        let entries = BufReader::new(File::open(path)?)
            .lines()
            .filter_map(|line| match line {
                Ok(line) if line.trim().is_empty() => None,
                Ok(line) => {
                    Some(serde_json::from_str::<ActionLogEntry>(&line).map_err(StorageError::from))
                }
                Err(error) => Some(Err(StorageError::from(error))),
            })
            .collect::<Result<Vec<_>>>()?;

        if let Some(limit) = limit {
            let start = entries.len().saturating_sub(limit);
            Ok(entries[start..].to_vec())
        } else {
            Ok(entries)
        }
    }

    /// 返回某个 job 的所有 checkpoint 文件路径，按文件名（即 checkpoint_id）升序。
    pub fn list_checkpoint_files(&self, job_id: &str) -> Result<Vec<PathBuf>> {
        let dir = self.job_dir(job_id).join(CHECKPOINTS_DIR);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut files = fs::read_dir(dir)?
            .filter_map(|entry| match entry {
                Ok(entry) => {
                    let path = entry.path();
                    (path.extension().and_then(|ext| ext.to_str()) == Some("json")).then_some(path)
                }
                Err(_) => None,
            })
            .collect::<Vec<_>>();
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        Ok(files)
    }

    /// 将 job 的 result.json 移动到 .kingdom/archive/{job_id}/result.json。
    pub fn archive_job(&self, job_id: &str) -> Result<()> {
        let src = self.job_dir(job_id).join(RESULT_FILE);
        let dst = self.root.join(ARCHIVE_DIR).join(job_id).join(RESULT_FILE);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(src, dst)?;
        Ok(())
    }

    /// 将 action log 中 cutoff 之前的条目压缩为摘要行，保留 cutoff 之后的条目。
    pub fn compress_action_log(&self, cutoff: DateTime<Utc>) -> Result<()> {
        let entries = self.read_action_log(None)?;
        let (old_entries, new_entries): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|entry| entry.timestamp < cutoff);
        if old_entries.is_empty() {
            return Ok(());
        }

        let date_from = old_entries
            .first()
            .map(|entry| entry.timestamp.to_rfc3339())
            .unwrap_or_else(|| cutoff.to_rfc3339());
        let date_to = old_entries
            .last()
            .map(|entry| entry.timestamp.to_rfc3339())
            .unwrap_or_else(|| cutoff.to_rfc3339());

        let mut max_tokens_by_worker: HashMap<String, u64> = HashMap::new();
        let mut compressed_tokens = 0_u64;
        for entry in &old_entries {
            if entry.action == "context.ping" {
                if let Some(token_count) = entry
                    .params
                    .get("token_count")
                    .and_then(|value| value.as_u64())
                {
                    let actor = entry.actor.clone();
                    max_tokens_by_worker
                        .entry(actor)
                        .and_modify(|current| *current = (*current).max(token_count))
                        .or_insert(token_count);
                }
            } else if entry.action == "compressed_summary" {
                compressed_tokens += entry
                    .params
                    .get("tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
            }
        }
        let tokens = compressed_tokens
            + max_tokens_by_worker
                .values()
                .copied()
                .sum::<u64>();

        let compressed_entry = ActionLogEntry {
            timestamp: cutoff,
            actor: "kingdom".to_string(),
            action: "compressed_summary".to_string(),
            params: json!({
                "date_from": date_from,
                "date_to": date_to,
                "count": old_entries.len(),
                "tokens": tokens,
            }),
            result: None,
            error: None,
        };

        let mut out = Vec::new();
        serde_json::to_writer(&mut out, &compressed_entry)?;
        out.push(b'\n');
        for entry in &new_entries {
            serde_json::to_writer(&mut out, entry)?;
            out.push(b'\n');
        }
        self.write_bytes_atomic(&self.root.join(LOGS_DIR).join(ACTION_LOG_FILE), &out)
    }

    /// 删除 job 所有 checkpoint 中，除最后一个之外、创建时间早于 cutoff 的 checkpoint 文件。
    pub fn delete_old_checkpoints(&self, job_id: &str, cutoff: DateTime<Utc>) -> Result<usize> {
        let files = self.list_checkpoint_files(job_id)?;
        let last = files.last().cloned();
        let mut deleted = 0;

        for path in files {
            if last.as_ref() == Some(&path) {
                continue;
            }
            let checkpoint: CheckpointContent = serde_json::from_slice(&fs::read(&path)?)?;
            if checkpoint.created_at < cutoff {
                fs::remove_file(path)?;
                deleted += 1;
            }
        }

        Ok(deleted)
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(STATE_FILE)
    }

    fn job_dir(&self, job_id: &str) -> PathBuf {
        self.root.join(JOBS_DIR).join(job_id)
    }

    fn checkpoint_path(&self, job_id: &str, checkpoint_id: &str) -> PathBuf {
        self.job_dir(job_id)
            .join(CHECKPOINTS_DIR)
            .join(format!("{checkpoint_id}.json"))
    }

    fn write_json_atomic<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        self.write_bytes_atomic(path, &bytes)
    }

    fn write_bytes_atomic(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp_path = path.with_extension("tmp");
        fs::write(&temp_path, bytes)?;
        fs::rename(temp_path, path)?;
        Ok(())
    }
}

fn render_handoff_markdown(brief: &HandoffBrief) -> String {
    let mut output = String::new();
    output.push_str(&format!("# Handoff for {}\n\n", brief.job_id));
    output.push_str(&format!(
        "## Original Intent\n{}\n\n",
        brief.original_intent
    ));
    output.push_str(&format!("## Done\n{}\n\n", brief.done));
    output.push_str(&format!("## In Progress\n{}\n\n", brief.in_progress));
    output.push_str(&format!("## Remaining\n{}\n\n", brief.remaining));
    output.push_str(&format!("## Pitfalls\n{}\n\n", brief.pitfalls));
    output.push_str("## Possibly Incomplete Files\n");
    if brief.possibly_incomplete_files.is_empty() {
        output.push_str("- None\n");
    } else {
        for path in &brief.possibly_incomplete_files {
            output.push_str(&format!("- {path}\n"));
        }
    }
    output.push_str("\n## Changed Files\n");
    if brief.changed_files.is_empty() {
        output.push_str("- None\n");
    } else {
        for path in &brief.changed_files {
            output.push_str(&format!("- {path}\n"));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CheckpointMeta, FailoverReason, GitStrategy, JobStatus, NoteScope, NotificationMode,
        PendingFailover, PendingFailoverStatus, PendingRequest, Worker, WorkerRole, WorkerStatus,
        WorkspaceNote,
    };
    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    fn ts() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 5, 12, 0, 0).unwrap()
    }

    fn sample_job() -> Job {
        Job {
            id: "job_001".to_string(),
            intent: "Implement M1".to_string(),
            status: JobStatus::Running,
            worker_id: Some("w1".to_string()),
            depends_on: vec!["job_000".to_string()],
            created_at: ts(),
            updated_at: ts(),
            branch: Some("kingdom/job_001".to_string()),
            branch_start_commit: Some("abc123".to_string()),
            checkpoints: vec![CheckpointMeta {
                id: "ckpt_20260405T120000".to_string(),
                job_id: "job_001".to_string(),
                created_at: ts(),
                git_commit: Some("abc123".to_string()),
            }],
            result: Some(JobResult {
                summary: "done".to_string(),
                changed_files: vec!["src/types.rs".to_string()],
                completed_at: ts(),
                worker_id: "w1".to_string(),
            }),
            fail_count: 1,
            last_fail_at: Some(ts()),
        }
    }

    fn sample_handoff() -> HandoffBrief {
        HandoffBrief {
            job_id: "job_001".to_string(),
            original_intent: "Implement M1".to_string(),
            done: "Defined types".to_string(),
            in_progress: "Writing storage".to_string(),
            remaining: "Run tests".to_string(),
            pitfalls: "Do not create jobs/*/meta.json".to_string(),
            possibly_incomplete_files: vec!["src/storage.rs".to_string()],
            changed_files: vec!["src/types.rs".to_string(), "src/storage.rs".to_string()],
        }
    }

    fn sample_session() -> Session {
        let worker = Worker {
            id: "w1".to_string(),
            provider: "codex".to_string(),
            role: WorkerRole::Worker,
            status: WorkerStatus::Idle,
            job_id: Some("job_001".to_string()),
            pid: Some(123),
            pane_id: "%1".to_string(),
            mcp_connected: true,
            context_usage_pct: Some(0.3),
            token_count: Some(100),
            last_heartbeat: Some(ts()),
            last_progress: Some(ts()),
            permissions: vec![],
            started_at: ts(),
        };
        let request = PendingRequest {
            id: "req_001".to_string(),
            job_id: "job_001".to_string(),
            worker_id: "w1".to_string(),
            question: "question".to_string(),
            blocking: true,
            answer: None,
            answered: false,
            created_at: ts(),
            answered_at: None,
        };
        let failover = PendingFailover {
            worker_id: "w1".to_string(),
            job_id: "job_001".to_string(),
            reason: FailoverReason::Manual,
            handoff_brief: sample_handoff(),
            recommended_provider: Some("claude".to_string()),
            created_at: ts(),
            status: PendingFailoverStatus::WaitingConfirmation,
        };

        Session {
            id: "sess_a3f9c2b1".to_string(),
            workspace_path: "/tmp/workspace".to_string(),
            workspace_hash: "a3f9c2".to_string(),
            manager_id: Some("w0".to_string()),
            workers: [(worker.id.clone(), worker)].into_iter().collect(),
            jobs: [("job_001".to_string(), sample_job())]
                .into_iter()
                .collect(),
            notes: vec![WorkspaceNote {
                id: "note_001".to_string(),
                content: "note".to_string(),
                scope: NoteScope::Global,
                created_at: ts(),
            }],
            worker_seq: 2,
            job_seq: 2,
            request_seq: 2,
            git_strategy: GitStrategy::Branch,
            available_providers: vec!["codex".to_string()],
            notification_mode: NotificationMode::Push,
            pending_requests: [(request.id.clone(), request)].into_iter().collect(),
            pending_failovers: [(failover.worker_id.clone(), failover)]
                .into_iter()
                .collect(),
            provider_stability: [(
                "codex".to_string(),
                crate::types::ProviderStability {
                    provider: "codex".to_string(),
                    crash_count: 1,
                    timeout_count: 0,
                    last_failure_at: Some(ts()),
                },
            )]
            .into_iter()
            .collect(),
            created_at: ts(),
        }
    }

    #[test]
    fn init_creates_layout_for_new_and_existing_directory() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        assert!(storage.root.join(".gitignore").exists());
        assert!(storage.root.join("logs").join("action.jsonl").exists());
        assert!(storage.root.join("jobs").exists());

        let storage_again = Storage::init(temp.path()).unwrap();
        assert_eq!(storage, storage_again);
    }

    #[test]
    fn session_round_trip_persists_request_and_failover_fields() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let session = sample_session();

        storage.save_session(&session).unwrap();
        let loaded = storage.load_session().unwrap().unwrap();

        assert_eq!(loaded, session);
        assert_eq!(loaded.request_seq, 2);
        assert!(loaded.pending_requests.contains_key("req_001"));
        assert!(loaded.pending_failovers.contains_key("w1"));
    }

    #[test]
    fn job_round_trip_reads_from_state_json_only() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let session = sample_session();
        storage.save_session(&session).unwrap();

        let meta_path = storage.root.join("jobs").join("job_001").join("meta.json");
        fs::create_dir_all(meta_path.parent().unwrap()).unwrap();
        fs::write(&meta_path, br#"{"id":"wrong"}"#).unwrap();

        let loaded = storage.load_job("job_001").unwrap().unwrap();
        assert_eq!(loaded, sample_job());
    }

    #[test]
    fn save_job_updates_state_json() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let session = sample_session();
        storage.save_session(&session).unwrap();

        let mut job = sample_job();
        job.status = JobStatus::Cancelled;
        storage.save_job(&job).unwrap();

        let loaded = storage.load_session().unwrap().unwrap();
        assert_eq!(loaded.jobs.get("job_001"), Some(&job));
    }

    #[test]
    fn checkpoint_round_trip_works() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let checkpoint = CheckpointContent {
            id: "ckpt_20260405T120000".to_string(),
            job_id: "job_001".to_string(),
            created_at: ts(),
            done: "done".to_string(),
            abandoned: "abandoned".to_string(),
            in_progress: "progress".to_string(),
            remaining: "remaining".to_string(),
            pitfalls: "pitfalls".to_string(),
            git_commit: Some("abc123".to_string()),
        };

        storage.save_checkpoint(&checkpoint).unwrap();
        let loaded = storage
            .load_checkpoint("job_001", "ckpt_20260405T120000")
            .unwrap();

        assert_eq!(loaded, checkpoint);
    }

    #[test]
    fn action_log_appends_and_reads_entries() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let first = ActionLogEntry {
            timestamp: ts(),
            actor: "w1".to_string(),
            action: "job.progress".to_string(),
            params: serde_json::json!({"job_id": "job_001"}),
            result: None,
            error: None,
        };
        let second = ActionLogEntry {
            timestamp: ts(),
            actor: "w1".to_string(),
            action: "job.complete".to_string(),
            params: serde_json::json!({"job_id": "job_001"}),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        };

        storage.append_action_log(&first).unwrap();
        storage.append_action_log(&second).unwrap();

        let entries = storage.read_action_log(None).unwrap();
        assert_eq!(entries, vec![first.clone(), second.clone()]);
        let limited = storage.read_action_log(Some(1)).unwrap();
        assert_eq!(limited, vec![second]);
    }

    #[test]
    fn save_result_and_handoff_create_expected_files() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let result = JobResult {
            summary: "done".to_string(),
            changed_files: vec!["src/storage.rs".to_string()],
            completed_at: ts(),
            worker_id: "w1".to_string(),
        };
        let handoff = sample_handoff();

        storage.save_result("job_001", &result).unwrap();
        storage.save_handoff("job_001", &handoff).unwrap();

        let result_path = storage
            .root
            .join("jobs")
            .join("job_001")
            .join("result.json");
        let handoff_path = storage.root.join("jobs").join("job_001").join("handoff.md");

        assert!(result_path.exists());
        assert!(handoff_path.exists());
        let stored: JobResult = serde_json::from_slice(&fs::read(result_path).unwrap()).unwrap();
        assert_eq!(stored, result);
        let markdown = fs::read_to_string(handoff_path).unwrap();
        assert!(markdown.contains("## Original Intent"));
        assert!(markdown.contains("Do not create jobs/*/meta.json"));
    }

    #[test]
    fn test_archive_job() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        storage
            .save_result(
                "job_001",
                &JobResult {
                    summary: "done".to_string(),
                    changed_files: vec![],
                    completed_at: ts(),
                    worker_id: "w1".to_string(),
                },
            )
            .unwrap();

        storage.archive_job("job_001").unwrap();

        assert!(!storage.root.join("jobs/job_001/result.json").exists());
        assert!(storage.root.join("archive/job_001/result.json").exists());
    }

    #[test]
    fn test_compress_action_log() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let old_1 = ActionLogEntry {
            timestamp: Utc.with_ymd_and_hms(2026, 4, 1, 10, 0, 0).unwrap(),
            actor: "w1".to_string(),
            action: "context.ping".to_string(),
            params: json!({"token_count": 100, "job_id": "job_001"}),
            result: None,
            error: None,
        };
        let old_2 = ActionLogEntry {
            timestamp: Utc.with_ymd_and_hms(2026, 4, 1, 11, 0, 0).unwrap(),
            actor: "w1".to_string(),
            action: "context.ping".to_string(),
            params: json!({"token_count": 150, "job_id": "job_001"}),
            result: None,
            error: None,
        };
        let new_entry = ActionLogEntry {
            timestamp: Utc.with_ymd_and_hms(2026, 4, 6, 11, 0, 0).unwrap(),
            actor: "w2".to_string(),
            action: "job.progress".to_string(),
            params: json!({"job_id": "job_002"}),
            result: None,
            error: None,
        };
        storage.append_action_log(&old_1).unwrap();
        storage.append_action_log(&old_2).unwrap();
        storage.append_action_log(&new_entry).unwrap();

        let cutoff = Utc.with_ymd_and_hms(2026, 4, 5, 0, 0, 0).unwrap();
        storage.compress_action_log(cutoff).unwrap();

        let entries = storage.read_action_log(None).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].action, "compressed_summary");
        assert_eq!(entries[0].timestamp, cutoff);
        assert_eq!(entries[0].params["count"], json!(2));
        assert_eq!(entries[0].params["tokens"], json!(150));
        assert_eq!(entries[1], new_entry);
    }

    #[test]
    fn test_delete_old_checkpoints_keeps_last() {
        let temp = tempdir().unwrap();
        let storage = Storage::init(temp.path()).unwrap();
        let first = CheckpointContent {
            id: "ckpt_1".to_string(),
            job_id: "job_001".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 3, 1, 10, 0, 0).unwrap(),
            done: "one".to_string(),
            abandoned: String::new(),
            in_progress: String::new(),
            remaining: String::new(),
            pitfalls: String::new(),
            git_commit: None,
        };
        let second = CheckpointContent {
            id: "ckpt_2".to_string(),
            job_id: "job_001".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 3, 2, 10, 0, 0).unwrap(),
            done: "two".to_string(),
            abandoned: String::new(),
            in_progress: String::new(),
            remaining: String::new(),
            pitfalls: String::new(),
            git_commit: None,
        };
        storage.save_checkpoint(&first).unwrap();
        storage.save_checkpoint(&second).unwrap();

        let deleted = storage
            .delete_old_checkpoints(
                "job_001",
                Utc.with_ymd_and_hms(2026, 3, 10, 0, 0, 0).unwrap(),
            )
            .unwrap();

        assert_eq!(deleted, 1);
        assert!(!storage.root.join("jobs/job_001/checkpoints/ckpt_1.json").exists());
        assert!(storage.root.join("jobs/job_001/checkpoints/ckpt_2.json").exists());
    }
}
