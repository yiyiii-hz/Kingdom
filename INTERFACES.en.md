# Kingdom v2 Interface Contract

> This document is the implementation "constitution." All Rust types, MCP tool signatures, and file formats are defined here.
> Codex implementations must use the types here and **must not change the signatures**. Any signature change requires manager approval and an update to this document.

---

## Core Data Types

### Job

```rust
pub struct Job {
    pub id: String,                        // format: "job_001", increments within a session
    pub intent: String,                    // original user description
    pub status: JobStatus,
    pub worker_id: Option<String>,         // currently assigned worker
    pub depends_on: Vec<String>,           // list of job_id values
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub branch: Option<String>,            // git branch, set when strategy=branch
    pub branch_start_commit: Option<String>, // HEAD commit hash when the branch was created, used to compute changed_files
    pub checkpoints: Vec<CheckpointMeta>,  // list of checkpoint metadata (not full text)
    pub result: Option<JobResult>,
    pub fail_count: u32,                   // used for circuit-breaker counting
    pub last_fail_at: Option<DateTime<Utc>>,
}

pub enum JobStatus {
    Pending,      // dependencies satisfied, waiting for manager assignment
    Waiting,      // has unfinished dependencies
    Running,      // worker is executing
    Completed,    // finished (job.complete called)
    Failed,       // unexpected failure (crash / timeout / API error)
    Cancelled,    // user-initiated cancellation (job.cancel + job.cancelled())
    Paused,       // circuit breaker or user pause; recoverable (job.keep_waiting / manual resume)
    Cancelling,   // cancellation in progress (waiting for graceful worker stop; becomes Cancelled after 30s timeout)
}

// Status semantics:
// - Failed and Cancelled have the same effect on dependents: dependents remain Waiting, and the manager is notified to decide next steps
// - Cancelled does not preserve result.json
// - Paused preserves all checkpoints and can be reactivated by job.update

pub struct JobResult {
    pub summary: String,
    pub changed_files: Vec<String>,
    pub completed_at: DateTime<Utc>,
    pub worker_id: String,
}
```

### Checkpoint

```rust
pub struct CheckpointMeta {
    pub id: String,
    pub job_id: String,
    pub created_at: DateTime<Utc>,
    pub git_commit: Option<String>,        // commit hash corresponding to the checkpoint
}

// Full content is stored on disk, not in memory
pub struct CheckpointContent {
    pub id: String,
    pub job_id: String,
    pub created_at: DateTime<Utc>,
    pub done: String,                      // what was done
    pub abandoned: String,                 // what was abandoned and why
    pub in_progress: String,               // what is being worked on now
    pub remaining: String,                 // what remains
    pub pitfalls: String,                  // pitfalls encountered
    pub git_commit: Option<String>,
}
```

### Worker

```rust
pub struct Worker {
    pub id: String,                        // format: "w1", increments within a session, never reused
    pub provider: String,                  // "claude" | "codex" | "gemini"
    pub role: WorkerRole,
    pub status: WorkerStatus,
    pub job_id: Option<String>,
    pub pid: Option<u32>,
    pub pane_id: String,                   // tmux pane id
    pub mcp_connected: bool,
    pub context_usage_pct: Option<f32>,    // 0.0 - 1.0
    pub token_count: Option<u64>,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub last_progress: Option<DateTime<Utc>>,
    pub permissions: Vec<Permission>,      // currently granted extended permissions
    pub started_at: DateTime<Utc>,
}

pub enum WorkerRole {
    Manager,
    Worker,
}

pub enum WorkerStatus {
    Starting,
    Running,
    Idle,
    Failed,
    Terminated,
}

pub enum Permission {
    SubtaskCreate,
    WorkerNotify,
    WorkspaceRead,
    JobReadAll,
}
```

### Session

```rust
pub struct Session {
    pub id: String,
    pub workspace_path: String,
    pub workspace_hash: String,             // path hash, used for socket naming
    pub manager_id: Option<String>,         // manager worker id
    pub workers: HashMap<String, Worker>,
    pub jobs: HashMap<String, Job>,
    pub notes: Vec<WorkspaceNote>,
    pub worker_seq: u32,                    // next worker sequence number (w{seq})
    pub job_seq: u32,                       // next job sequence number (job_{seq:03})
    pub request_seq: u32,                   // next request sequence number (req_{seq:03})
    pub git_strategy: GitStrategy,
    pub available_providers: Vec<String>,   // detected installed providers
    pub notification_mode: NotificationMode,
    pub pending_requests: HashMap<String, PendingRequest>,   // key = request_id
    pub pending_failovers: HashMap<String, PendingFailover>, // key = worker_id (at most one pending failover per worker)
    pub created_at: DateTime<Utc>,
}
```

### PendingRequest

```rust
pub struct PendingRequest {
    pub id: String,                         // format: req_001
    pub job_id: String,
    pub worker_id: String,                  // worker that initiated the request
    pub question: String,
    pub blocking: bool,
    pub answer: Option<String>,
    pub answered: bool,
    pub created_at: DateTime<Utc>,
    pub answered_at: Option<DateTime<Utc>>,
}
```

### PendingFailover

```rust
pub struct PendingFailover {
    pub worker_id: String,
    pub job_id: String,
    pub reason: FailoverReason,
    pub handoff_brief: HandoffBrief,
    pub recommended_provider: Option<String>,
    pub created_at: DateTime<Utc>,
    pub status: PendingFailoverStatus,
}

pub enum PendingFailoverStatus {
    WaitingConfirmation,               // waiting for manager/user confirmation
    Confirmed { new_provider: String },
    Cancelled,
}

pub enum GitStrategy {
    Branch,
    Commit,
    None,
}

pub enum NotificationMode {
    Push,    // MCP server→client notification
    Poll,    // worker polls job.request_status
}
```

### WorkspaceNote

```rust
pub struct WorkspaceNote {
    pub id: String,
    pub content: String,
    pub scope: NoteScope,
    pub created_at: DateTime<Utc>,
}

pub enum NoteScope {
    Global,
    Directory(String),
    Job(String),
}
```

### Failover

```rust
pub struct FailoverRequest {
    pub id: String,
    pub worker_id: String,
    pub job_id: String,
    pub reason: FailoverReason,
    pub handoff_brief: HandoffBrief,
    pub recommended_provider: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub enum FailoverReason {
    Network,
    ContextLimit,
    ProcessExit { exit_code: i32 },
    HeartbeatTimeout,
    RateLimit,
    Manual,
}

pub struct HandoffBrief {
    pub job_id: String,
    pub original_intent: String,
    pub done: String,
    pub in_progress: String,
    pub remaining: String,
    pub pitfalls: String,
    pub possibly_incomplete_files: Vec<String>,  // files being written when the crash occurred
    pub changed_files: Vec<String>,
}
```

### Health Events

```rust
pub enum HealthEvent {
    HeartbeatMissed {
        worker_id: String,
        consecutive_count: u32,
    },
    ProcessExited {
        worker_id: String,
        exit_code: i32,
    },
    ContextThreshold {
        worker_id: String,
        pct: f32,
        urgency: CheckpointUrgency,
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

pub enum CheckpointUrgency {
    Normal,    // 50%
    High,      // 70%
    Critical,  // 85%
}
```

### Manager Notification

```rust
pub enum ManagerNotification {
    JobCompleted {
        job_id: String,
        worker_id: String,      // worker that completed the job
        summary: String,
        changed_files: Vec<String>,
    },
    JobFailed {
        job_id: String,
        worker_id: String,
        reason: String,
    },
    WorkerRequest {
        job_id: String,
        request_id: String,
        question: String,
        blocking: bool,
    },
    JobUnblocked {
        job_id: String,
    },
    FailoverReady {
        worker_id: String,
        reason: FailoverReason,
        candidates: Vec<String>,  // recommended provider list
    },
    WorkerIdle {
        worker_id: String,
    },
    WorkerReady {
        worker_id: String,
        provider: String,         // kingdom.hello handshake completed, Starting → Idle
    },
    SubtaskCreated {
        parent_job_id: String,
        subtask_job_id: String,
        intent: String,
    },
    CancelCascade {
        cancelled_job_id: String,
        affected_jobs: Vec<String>,
    },
    ProgressWarning {
        worker_id: String,
        job_id: String,
        elapsed_minutes: u32,
    },
}
```

### Action Log

```rust
// Appends to .kingdom/logs/action.jsonl, one JSON object per line
pub struct ActionLogEntry {
    pub timestamp: DateTime<Utc>,
    pub actor: String,     // worker_id | "kingdom" | "user"
    pub action: String,    // "job.complete" | "failover" | "worker.create" etc.
    pub params: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}
```

---

## MCP Tool Signatures

> All parameter and return types must match exactly as defined here.

### Manager Tool Set

```
workspace.status() -> WorkspaceStatus
workspace.log(limit?: u32) -> Vec<ActionLogEntry>
workspace.note(content: String, scope?: String) -> String   // returns note_id
workspace.notes() -> Vec<WorkspaceNote>

worker.create(provider: String) -> String                   // returns worker_id
worker.assign(worker_id: String, job_id: String) -> ()
worker.release(worker_id: String) -> ()                     // may only be called on an idle worker
worker.swap(worker_id: String, new_provider: String) -> ()
worker.grant(worker_id: String, permission: String) -> ()
worker.revoke(worker_id: String, permission: String) -> ()

job.create(
    intent: String,
    worker_id?: String,       // if provided, auto-assign
    depends_on?: Vec<String>
) -> String                                                 // returns job_id

job.status(job_id: String) -> JobStatusResponse
job.result(job_id: String) -> JobResultResponse             // only meaningful after completion
job.cancel(job_id: String) -> ()
job.keep_waiting(job_id: String) -> ()
job.update(job_id: String, new_intent: String) -> ()
job.respond(request_id: String, answer: String) -> ()

failover.confirm(worker_id: String, new_provider: String) -> ()
failover.cancel(worker_id: String) -> ()
```

### Worker Tool Set (Default Minimum Permissions)

```
job.progress(job_id: String, note: String) -> ()
job.complete(job_id: String, result_summary: String) -> ()  // idempotent
job.fail(job_id: String, reason: String) -> ()
job.cancelled() -> ()
job.checkpoint(job_id: String, summary: CheckpointSummary) -> ()
job.request(
    job_id: String,
    question: String,
    blocking: bool
) -> String                                                 // returns request_id
job.request_status(request_id: String) -> RequestStatus
job.status(job_id: String) -> JobStatusResponse             // may only query its own job

file.read(path: String, lines?: String, symbol?: String) -> String
workspace.tree(path?: String) -> String
git.log(n?: u32) -> Vec<GitLogEntry>
git.diff(path?: String) -> String                          // returns unified diff

context.ping(usage_pct: f32, token_count: u64) -> ()
context.checkpoint_defer(
    job_id: String,
    reason: String,
    eta_seconds: u32
) -> ()
```

### Worker Extended Tools (Requires Authorization)

```
subtask.create(
    intent: String,
    depends_on?: Vec<String>
) -> String                                                 // returns job_id

worker.notify(
    target_worker_id: String,
    message: String
) -> ()

workspace.status() -> WorkspaceStatus                      // workspace.read permission
job.list_all() -> Vec<JobSummary>                          // job.read_all permission
```

---

## Return Value Structures

```rust
pub struct WorkspaceStatus {
    pub session_id: String,
    pub manager: Option<WorkerSummary>,
    pub workers: Vec<WorkerSummary>,
    pub jobs: Vec<JobSummary>,
    pub notes: Vec<WorkspaceNote>,
}

pub struct WorkerSummary {
    pub id: String,
    pub provider: String,
    pub status: WorkerStatus,
    pub job_id: Option<String>,
    pub context_pct: Option<f32>,
}

pub struct JobSummary {
    pub id: String,
    pub intent: String,
    pub status: JobStatus,
    pub worker_id: Option<String>,
    pub depends_on: Vec<String>,
    pub created_at: DateTime<Utc>,
}

pub struct JobStatusResponse {
    pub id: String,
    pub status: JobStatus,
    pub worker_id: Option<String>,
    pub checkpoint_count: u32,
    pub last_progress: Option<DateTime<Utc>>,
}

pub struct JobResultResponse {
    pub id: String,
    pub summary: String,
    pub changed_files: Vec<String>,
    pub checkpoint_count: u32,
    pub branch: Option<String>,
    pub completed_at: DateTime<Utc>,
}

pub struct RequestStatus {
    pub request_id: String,
    pub answered: bool,
    pub answer: Option<String>,
}

pub struct CheckpointSummary {
    pub done: String,          // ≥20 chars
    pub abandoned: String,     // ≥20 chars
    pub in_progress: String,   // ≥20 chars
    pub remaining: String,     // ≥20 chars
    pub pitfalls: String,      // ≥20 chars
}

pub struct GitLogEntry {
    pub hash: String,
    pub message: String,
    pub author: String,
    pub timestamp: DateTime<Utc>,
}
```

---

## File Formats

### `.kingdom/state.json` - the single source of truth

**`state.json` is the only authoritative source.** The full session state, including all Job, Worker, PendingRequest, and PendingFailover data, lives here.

- All write operations must update `state.json` first, then perform any other actions
- On daemon startup recovery, read only `state.json`; do not read `jobs/*/meta.json`
- `jobs/*/meta.json` does **not** exist anymore (it has been removed to avoid double writes and inconsistencies)

```json
{
  "session_id": "sess_abc123",
  "workspace_path": "/Users/yang/project",
  "workspace_hash": "a3f9c2",
  "manager_id": "w0",
  "worker_seq": 3,
  "job_seq": 5,
  "request_seq": 2,
  "git_strategy": "branch",
  "available_providers": ["claude", "codex"],
  "notification_mode": "push",
  "workers": { "w0": { ... }, "w1": { ... } },
  "jobs": { "job_001": { ... } },
  "notes": [ { ... } ],
  "pending_requests": { "req_001": { ... } },
  "pending_failovers": { "w1": { ... } },
  "created_at": "2026-04-05T12:00:00Z"
}
```

### `.kingdom/jobs/{job_id}/`

Stores only content that is too large or not suitable for `state.json`. It is not a source of truth.

```
result.json        JobResult (written after completion; synchronized into Job.result in state.json)
handoff.md         latest handoff brief (Markdown, for humans only)
checkpoints/
  {id}.json        CheckpointContent (one full file per checkpoint)
```

> Note: `Job.checkpoints` in `state.json` stores only `Vec<CheckpointMeta>` (lightweight metadata),
> while the full checkpoint content lives in `jobs/{job_id}/checkpoints/{id}.json`.
> `Job.result` in `state.json` stores the full `JobResult` (bounded size), and is also written to `result.json` for human reading.

### `.kingdom/logs/action.jsonl`

Append-only, one ActionLogEntry JSON per line.

### `.kingdom/manager.json` / `.kingdom/worker.json`

MCP config files generated by `kingdom up`:

```json
{
  "mcpServers": {
    "kingdom": {
      "transport": "unix",
      "socket": "/tmp/kingdom/a3f9c2.sock",
      "role": "manager",
      "session_id": "sess_abc123"
    }
  }
}
```

### `/tmp/kingdom/{workspace_hash}.sock`

Unix domain socket listening address for the Kingdom MCP server.

---

## Configuration Format (`.kingdom/config.toml`)

```toml
[workers]
main_window_max = 3            # maximum number of workers in the main window

[git]
strategy = "branch"            # branch | commit | none
branch_prefix = "kingdom/"
auto_commit = false

[health]
heartbeat_interval_seconds = 30
heartbeat_timeout_count = 2
progress_timeout_minutes = 30

[failover]
window_minutes = 10
failure_threshold = 3
cooldown_seconds = 30

[idle]
timeout_minutes = 10

[notifications]
on_job_complete = "none"       # none | bell | system
on_attention_required = "bell"
on_job_failed = "system"

[tmux]
session_name = "kingdom"

[storage]
checkpoint_retention_days = 7
job_result_retention_days = 90
action_log_retention_days = 30

[providers.claude]
binary = "claude"
args = ["--mcp-config", "{mcp_config}"]

[providers.codex]
binary = "codex"
args = ["--mcp-config", "{mcp_config}"]

[providers.gemini]
binary = "gemini"
args = ["--mcp-config", "{mcp_config}"]
```

---

## ID Format Specification

| Entity | Format | Example | Rule |
|---|---|---|---|
| Job | `job_{seq:03}` | `job_001` | increments within a session, does not reset across sessions (read from state.json) |
| Worker | `w{seq}` | `w1` | increments within a session, never reused; resets to 1 on `kingdom up` |
| Session | `sess_{hash8}` | `sess_a3f9c2b1` | random 8-character hex |
| Checkpoint | `ckpt_{timestamp}` | `ckpt_20260405T143201` | ISO timestamp |
| Note | `note_{timestamp}` | `note_20260405T143201` | ISO timestamp |
| Request | `req_{seq:03}` | `req_001` | increments within a session |
| Failover | `fov_{timestamp}` | `fov_20260405T143201` | ISO timestamp |

---

## MCP Protocol Definition

> This section defines all JSON-RPC method names, payload structures, and idempotency rules.

### Connection Handshake (`kingdom.hello`)

After the provider connects to the Unix socket, the **first message must be** `kingdom.hello`:

```json
// Provider → Kingdom (Request)
{
  "jsonrpc": "2.0",
  "id": "init",
  "method": "kingdom.hello",
  "params": {
    "role": "worker",          // "manager" | "worker"
    "worker_id": "w1",         // when an existing worker reconnects, include the original worker_id (it must already be registered via worker.create)
    "session_id": "sess_abc123"
  }
}

// Kingdom → Provider (Response)
{
  "jsonrpc": "2.0",
  "id": "init",
  "result": {
    "tools": ["job.progress", "job.complete", ...],   // tools available to this role
    "notification_mode": "push",                      // "push" | "poll"
    "queued_notifications": [                         // notifications queued during disconnection, in chronological order
      // Each item has the same format as a kingdom.event notification:
      // { "method": "kingdom.event", "params": { "type": "...", "data": {...}, "text": "..." } }
      // Directive notifications (such as kingdom.cancel_job) do not enter this queue (the job is already Cancelled and does not need replay)
    ]
  }
}
```

**Rules for binding a Kingdom connection to a worker_id:**
- `worker_id` exists in state.json → reconnect, update `worker.mcp_connected = true`
- `worker_id` does not exist → reject with an error (a new worker must be registered first via `worker.create`)
- The old connection for the same `worker_id` is disconnected automatically (the new connection replaces it)

**If `session_id` does not match:** return an error and make the provider exit (cross-session connection reuse is not allowed).

---

### Cancellation Protocol (`kingdom.cancel_job`)

```
After `job.cancel(job_id)` is called by the manager:

1. Kingdom: job.status → Cancelling, write state.json
2. Kingdom → Worker (Notification, no id):
   {
     "jsonrpc": "2.0",
     "method": "kingdom.cancel_job",
     "params": { "job_id": "job_001" }
   }
3. After receiving it, the worker:
   - finishes the current atomic operation (must not truncate file writes midway)
   - calls job.cancelled()
   - does not call any other job.* tools
4. After Kingdom receives job.cancelled():
   - job.status → Cancelled
   - git stash (when strategy != None, run `git stash push -m "[kingdom cancelled] job_001"`)
   - worker.status → Idle
   - write state.json + action log

Timeout rule (30 seconds):
- If job.cancelled() is not received within 30 seconds → Kingdom sends SIGKILL
- job.status → Cancelled (forced)
- git stash (best effort, may be incomplete)
- worker.status → Terminated

job.cancelled() is idempotent: repeated calls return Ok and do not change state.
```

**Worker behavior constraints for `kingdom.cancel_job`:**
- After receiving the notification, the worker may finish the current tool call (for example, file.read), but must not start any new tool call
- After receiving cancellation, the worker must not call job.complete or job.checkpoint

---

### Reconnect and Replay Deduplication

**Provider side (reconnect after disconnection):**
- When the connection drops, cache all pending tool calls that have not yet received a Response (method + params + original id)
- After a successful reconnect handshake (`kingdom.hello`), resend the cached tool calls in their original order, using the **same id**

**Kingdom side (deduplication):**

```rust
// In-memory structure, not persisted
pub struct RecentCalls {
    // key: (worker_id, jsonrpc_id), where jsonrpc_id = the JSON-RPC message "id" field
    // Do not introduce an extra call_id field; reuse the JSON-RPC id directly
    cache: HashMap<(String, String), serde_json::Value>,
    // TTL: 5 minutes; entries are automatically removed after that
}
```

- When a tool call is received: first check RecentCalls, key = `(worker_id, message "id" field)`
  - Hit → return the cached result directly, **do not execute again**
  - Miss → execute and store in the cache
- After Kingdom restarts, RecentCalls is cleared (the in-memory cache is not persisted); on provider reconnect, `kingdom.hello`'s `queued_notifications` ensures notifications are not lost, and tool-call replay stays within the 5-minute window

**Handling TTL expiration:**
- Replay after more than 5 minutes → treat as a new call and execute it (a reconnect taking longer than 5 minutes is outside the normal reconnect scenario)

---

### `worker.assign` Legal State Constraints

```
Valid call conditions:
  - worker.role == Worker (manager cannot be assigned)
  - worker.status == Idle
  - job.status == Pending

All other cases return an error:

  worker.role == Manager
    → McpError::Unauthorized { reason: "cannot assign job to manager" }

  worker.status == Running (already has a job)
    → McpError::InvalidState { expected: "Idle", actual: "Running", detail: "worker already has job {job_id}" }

  worker.status == Starting
    → McpError::InvalidState { expected: "Idle", actual: "Starting", detail: "worker not yet ready" }

  worker.status == Terminated | Failed
    → McpError::InvalidState { expected: "Idle", actual: "{status}" }

  job.status != Pending
    → McpError::InvalidState { expected: "Pending", actual: "{status}" }
```

> A worker in Starting state may not be assigned early. The manager should wait for the `WorkerRunning` notification before calling assign.

---

### `job.cancel` Final State and Semantics

```
State transitions (the only allowed rule):

  User calls job.cancel(job_id)
  ├─ job has a running worker → Cancelling
  │   ├─ worker calls job.cancelled() within 30s → Cancelled
  │   └─ 30s timeout → Cancelled (forced SIGKILL)
  └─ job has no running worker (Pending/Waiting/Paused) → directly Cancelled

Semantics of the Cancelled state:
  - Do not write result.json
  - Job.result = None
  - For dependents: treated as "not completed"; dependents remain Waiting
  - manager receives ManagerNotification::CancelCascade (if there are dependents)

Failed vs Cancelled vs Paused:
  Failed   = unexpected failure (crash / heartbeat timeout / API error)
  Cancelled = user-initiated cancellation
  Paused   = circuit breaker or awaiting recovery (job.keep_waiting can reactivate)

  All three have the same effect on dependents: dependents remain Waiting, and the manager decides the next step.
```

---

### `changed_files` Computation Rules

```
strategy = "branch"：
  Record Job.branch_start_commit = git rev-parse HEAD when the branch is created
  At job.complete: git diff --name-only {branch_start_commit}..HEAD
  checkpoint commits are included in this range (correct)

strategy = "commit"：
  Record Job.branch_start_commit = git rev-parse HEAD at job start (on the current branch)
  At job.complete: git diff --name-only {branch_start_commit}..HEAD

strategy = "none" (including non-git directories)：
  Kingdom cannot compute changed_files
  worker may describe the modified files in result_summary itself (text only, not verified)
  JobResult.changed_files = [] (empty list; worker-reported files are not accepted)
```

---

### `context.checkpoint_request` Protocol

The checkpoint request Kingdom proactively sends to a worker is a **Kingdom → Worker JSON-RPC notification** (no id, no response required from the worker).

```json
// Kingdom → Worker (Notification, no id)
{
  "jsonrpc": "2.0",
  "method": "kingdom.checkpoint_request",
  "params": {
    "job_id": "job_001",
    "urgency": "Normal" | "High" | "Critical"
  }
}
```

**`urgency` type:** directly reuses the `CheckpointUrgency` enum:

```rust
pub enum CheckpointUrgency {
    Normal,    // context ≥ 50%, checkpoint recommended, may be deferred
    High,      // context ≥ 70%, strongly recommended, short deferral window
    Critical,  // context ≥ 85%, must checkpoint immediately, deferral not allowed
}
```

**Valid response paths after the worker receives it:**

| urgency | Allowed responses |
|---|---|
| Normal | Call `job.checkpoint()`, or call `context.checkpoint_defer(eta_seconds ≤ 60)` |
| High | Call `job.checkpoint()`, or call `context.checkpoint_defer(eta_seconds ≤ 15)` |
| Critical | Only `job.checkpoint()` is allowed; if `context.checkpoint_defer()` is called, Kingdom returns `ValidationFailed` |

The worker may also ignore the notification if it is currently in an atomic operation; Kingdom handles that via timeout detection.

**Timeouts and fallback on the Kingdom side:**

```
Kingdom sends kingdom.checkpoint_request(urgency)
  ↓
Waiting window (Normal: 60s / High: 15s / Critical: immediate, no wait)
  ↓
Worker calls job.checkpoint() within the window       → success, reset context threshold counter
Worker calls context.checkpoint_defer within the window → record delay, update next trigger time
Worker gives no response before timeout               → downgrade: Kingdom generates a fallback checkpoint using git diff
                                                       (leave all five fields empty, mark "[auto-generated, no summary]")
```

**Idempotency rules:**
- While the same worker is already waiting on a checkpoint, Kingdom does not resend `kingdom.checkpoint_request`
- The waiting state is reset only after the worker completes the checkpoint, allowing the next threshold trigger

**Relationship to `kingdom.cancel_job`:**
- If a worker receives `kingdom.checkpoint_request` and also receives `kingdom.cancel_job`, cancellation takes priority (finish the current atomic operation, then call `job.cancelled()`; no need to checkpoint first)