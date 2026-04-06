# Kingdom v2 Build Plan

> Chinese version: [BUILD_PLAN.zh.md](./BUILD_PLAN.zh.md)

> This document defines the implementation order and the acceptance criteria for each milestone.
> Codex should only work on one milestone at a time. After a milestone is complete, the manager verifies it before starting the next one.

---

## Technical Choices

### Language and Toolchain

```
Language: Rust (edition 2021)
Minimum version: 1.75.0 (supports async fn in trait)
```

### Project Structure

A single workspace with two binary crates:

```
Kingdom/
  Cargo.toml          workspace definition
  Cargo.lock
  crates/
    kingdom/          main program (daemon + CLI combined)
      Cargo.toml
      src/
        main.rs       entry point, parses subcommands
        cli/          CLI commands (up/down/log/doctor, etc.)
        mcp/          MCP server + toolset
        storage/      .kingdom/ read/write
        process/      provider startup + PID tracking
        health/       health monitoring
        failover/     failover state machine
        tmux/         tmux operations
        types.rs      all shared data types
        config.rs     config.toml parsing
    kingdom-watchdog/ watchdog process (standalone binary, very lightweight)
      Cargo.toml
      src/
        main.rs
```

The `kingdom` binary combines daemon and CLI:
- `kingdom up` → starts the watchdog, the watchdog starts the daemon, and the daemon runs in the background
- `kingdom log` / `kingdom doctor`, etc. → connect to the running daemon (via Unix socket) to read state

### Key Dependencies

```toml
[dependencies]
# async runtime
tokio = { version = "1", features = ["full"] }

# serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"

# CLI
clap = { version = "4", features = ["derive"] }

# error handling
anyhow = "1"
thiserror = "1"

# time
chrono = { version = "0.4", features = ["serde"] }

# process / syscalls
nix = { version = "0.27", features = ["process", "signal"] }

# logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# randomness (session ID generation)
rand = "0.8"

# Async trait dispatch (Tool trait needs Box<dyn Tool> + async fn, RPITIT does not support object safety)
async-trait = "0.1"

[dev-dependencies]
mockall = "0.12"
tempfile = "3"
tokio-test = "0.4"
```

### MCP Protocol Implementation Approach

**Do not use an external MCP SDK. Implement JSON-RPC 2.0 over Unix domain socket yourself.**

Reasons:
- The existing Rust MCP SDK (`rmcp`) has unstable Unix socket transport support
- Kingdom acts as both server (to provider) and client (sending notifications to provider), so a self-implemented solution gives fuller control
- The JSON-RPC protocol itself is extremely simple; a self-implementation is about 200 lines

Message format (standard JSON-RPC 2.0):

```json
// Request (sent or received by Kingdom)
{ "jsonrpc": "2.0", "id": 1, "method": "job.complete", "params": { ... } }

// Response
{ "jsonrpc": "2.0", "id": 1, "result": { ... } }

// Error
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32600, "message": "..." } }

// Notification (no id, one-way push from Kingdom → Provider)
// Event notifications use kingdom.event (informational; both manager and worker may receive them)
{ "jsonrpc": "2.0", "method": "kingdom.event", "params": { "type": "job_completed", "data": { ... } } }

// Command-style notifications use concrete method names (worker must perform a specific action)
{ "jsonrpc": "2.0", "method": "kingdom.cancel_job", "params": { "job_id": "job_001" } }
```

Implementation location: `crates/kingdom/src/mcp/jsonrpc.rs`

### Watchdog Design

`kingdom-watchdog` is a very lightweight standalone process (about 100 lines):

```rust
// Responsibility: start and monitor the kingdom daemon, restart immediately after a crash
// Does not manage state; all state lives in .kingdom/

fn main() {
    let daemon_path = std::env::args().nth(1).unwrap(); // kingdom daemon binary path
    let workspace = std::env::args().nth(2).unwrap();   // workspace path

    loop {
        let mut child = Command::new(&daemon_path)
            .arg("--daemon")
            .arg(&workspace)
            .spawn()
            .expect("failed to start daemon");

        let status = child.wait().unwrap();

        if status.success() {
            break; // normal exit (kingdom down), watchdog exits too
        }

        // abnormal exit → restart immediately
        eprintln!("[watchdog] daemon exited ({:?}), restarting...", status);
        std::thread::sleep(Duration::from_millis(500));
    }
}
```

The watchdog is started by `kingdom up`, and its PID is written to `.kingdom/watchdog.pid`.

## Overview

| Milestone | Name | Core Deliverable |
|---|---|---|
| M1 | Data Types + Storage Layer | Rust type definitions, `state.json` read/write |
| M2 | MCP Server Skeleton | Unix socket, tool dispatch, connection management |
| M3 | Manager Toolset | Implement all manager MCP tools |
| M4 | Worker Toolset | Implement all worker MCP tools |
| M5 | Process Manager | Provider startup, `kingdom up/down` |
| M6 | Health Monitoring | Heartbeats, PID monitoring, progress timeout |
| M7 | Failover State Machine | End-to-end detect → switch flow |
| M8 | tmux Integration | Status bar, popup, pane management |
| M9 | CLI Commands | `log` / `doctor` / `clean` / `cost` / `restart` |
| M10 | End-to-End Integration | Happy path + failover scenario tests |

## Dependency Order

```
M1 → M2 → M3 → M4 → M5 → M6 → M7
                              ↓
                    M8 → M9 → M10
```

M3 and M4 can run in parallel (after M2).

---

## M1: Data Types + Storage Layer

### Goal

Implement all Rust types defined in INTERFACES.md, plus read/write operations for the `.kingdom/` directory.
This is the foundation for all other milestones and does not include any networking, process, or tmux logic.

### Scope

**Type definitions (`src/types.rs`):**
- `Job`, `JobStatus`, `JobResult`, `CheckpointMeta`, `CheckpointContent`
- `Worker`, `WorkerRole`, `WorkerStatus`, `Permission`
- `Session`, `WorkspaceNote`, `NoteScope`, `GitStrategy`
- `FailoverRequest`, `FailoverReason`, `HandoffBrief`
- `PendingRequest`, `PendingFailover`, `PendingFailoverStatus` (INTERFACES.md §MCP protocol definition)
- `HealthEvent`, `CheckpointUrgency`
- `ManagerNotification`
- `ActionLogEntry`
- `RecentCalls` (replay dedup cache, `HashMap<(worker_id, jsonrpc_id), Instant>`)
- All return-value structs (`WorkspaceStatus`, `JobSummary`, `WorkerSummary`, etc.)
- All ID types (`String` newtype or alias)

**`JobStatus` enum must include a `Cancelled` variant** (user-initiated cancellation, semantically different from `Failed`, and auditable in history):
```rust
pub enum JobStatus {
    Pending, Waiting, Running, Completed,
    Failed,     // unexpected (crash / timeout / API error)
    Cancelled,  // user-initiated cancellation
    Paused,     // circuit breaker or user pause (resumable)
    Cancelling, // graceful stop in progress
}
```

**`ManagerNotification` enum must include a `WorkerReady` variant** (triggered when the worker finishes the `kingdom.hello` handshake and its status changes from Starting → Idle):
```rust
pub enum ManagerNotification {
    JobCompleted { job_id: String, worker_id: String, summary: String, changed_files: Vec<String> },
    JobFailed { job_id: String, worker_id: String, reason: String },
    WorkerRequest { job_id: String, request_id: String, question: String, blocking: bool },
    JobUnblocked { job_id: String },
    FailoverReady { worker_id: String, reason: FailoverReason, candidates: Vec<String> },
    WorkerIdle { worker_id: String },
    WorkerReady { worker_id: String, provider: String },  // Starting → Idle (handshake complete)
    SubtaskCreated { parent_job_id: String, subtask_job_id: String, intent: String },
}
```

**`Session` must include the following fields** (`state.json` is the single source of truth):
```rust
pub struct Session {
    // ... other fields ...
    pub request_seq: u32,
    pub pending_requests: HashMap<String, PendingRequest>,
    pub pending_failovers: HashMap<String, PendingFailover>,
}
```

All types must implement:
- `serde::Serialize` + `serde::Deserialize`
- `Debug`, `Clone`
- `PartialEq` (for test assertions)

**Storage layer (`src/storage.rs`):**

```rust
pub struct Storage {
    pub root: PathBuf,  // .kingdom/ directory path
}

impl Storage {
    pub fn init(workspace: &Path) -> Result<Self>;    // create .kingdom/ directory structure + .gitignore
    pub fn load_session() -> Result<Option<Session>>;
    pub fn save_session(session: &Session) -> Result<()>;
    pub fn load_job(job_id: &str) -> Result<Option<Job>>;
    pub fn save_job(job: &Job) -> Result<()>;
    pub fn load_checkpoint(job_id: &str, checkpoint_id: &str) -> Result<CheckpointContent>;
    pub fn save_checkpoint(content: &CheckpointContent) -> Result<()>;
    pub fn save_handoff(job_id: &str, brief: &HandoffBrief) -> Result<()>;
    pub fn save_result(job_id: &str, result: &JobResult) -> Result<()>;
    pub fn append_action_log(entry: &ActionLogEntry) -> Result<()>;
    pub fn read_action_log(limit: Option<usize>) -> Result<Vec<ActionLogEntry>>;
}
```

**Directory structure (`Storage::init` creates):**

```
.kingdom/
  .gitignore         contents: *
  state.json         single source of truth (no `jobs/*/meta.json`)
  logs/
    action.jsonl     initially empty
  jobs/              checkpoint + handoff + result files for each job
```

**State design constraint:** `state.json` is the only global source of truth. `jobs/*/meta.json` **does not exist**; all job metadata lives in the `jobs` field inside `state.json`. The `jobs/{id}/` directory stores only checkpoint/handoff/result and other large files. `Storage::load_job` reads from `state.json`, not from a separate file.

### Not Implemented

- MCP server / client
- Process startup
- tmux operations
- CLI commands

### Acceptance Criteria

- [ ] `cargo test` passes
- [ ] All types serialize and deserialize correctly in round-trip tests
- [ ] `Storage::init` works on both new and existing directories
- [ ] `save_session` + `load_session` round-trip correctly
- [ ] `save_job` + `load_job` round-trip correctly
- [ ] `save_checkpoint` + `load_checkpoint` round-trip correctly
- [ ] After `append_action_log`, `read_action_log` can read all entries
- [ ] Tests cover serialization of all state enums, including `JobStatus::Cancelled`
- [ ] `PendingRequest` + `PendingFailover` serialize and round-trip correctly
- [ ] `Session` can still round-trip with `pending_requests` / `pending_failovers` / `request_seq`
- [ ] `Storage::load_job` reads from `state.json` (does not depend on `jobs/*/meta.json`)

### Reference Docs

- `INTERFACES.md` §Core Data Types
- `INTERFACES.md` §File Formats
- `INTERFACES.md` §ID Format Specification
- `INTERFACES.md` §MCP Protocol Definition (`PendingRequest`, `PendingFailover`)
- `ARCHITECTURE.md` §Data Consistency

---

## M2: MCP Server Skeleton

### Goal

Implement the connection management and tool dispatch framework for the Kingdom MCP server.
Tools only need to be registered with skeleton implementations (return fake data); **no business logic is implemented yet**.

### Scope

**MCP Server (`src/mcp/server.rs`):**

```rust
pub struct McpServer {
    socket_path: PathBuf,
    // connection registry: connection_id → ConnectedClient
}

pub struct ConnectedClient {
    pub connection_id: String,
    pub worker_id: Option<String>,
    pub role: WorkerRole,
    pub session_id: String,
}

impl McpServer {
    pub fn new(workspace_hash: &str) -> Self;
    pub async fn start(&self) -> Result<()>;   // start listening on Unix socket
    pub async fn stop(&self) -> Result<()>;
}
```

**Connection handshake (`kingdom.hello` protocol):**

After connecting, the client must first send a `kingdom.hello` request. Only after Kingdom returns a response does the connection become ready:

```jsonc
// Client → Kingdom (exactly matches INTERFACES.md §MCP protocol definition)
{
  "jsonrpc": "2.0", "id": "init", "method": "kingdom.hello",
  "params": {
    "role": "manager" | "worker",
    "session_id": "sess_abc123",
    "worker_id": "w1"   // required for worker; omitted for manager. Reconnect and first connect use the same format, Kingdom checks state.json
  }
}

// Kingdom → Client (success)
{
  "jsonrpc": "2.0", "id": "init",
  "result": {
    "tools": ["job.progress", ...],   // tool list available to this role
    "notification_mode": "push" | "poll",
    "queued_notifications": [         // queued `kingdom.event` notifications during disconnect, in time order, may be empty
      { "method": "kingdom.event", "params": { "type": "...", "data": {}, "text": "..." } }
    ]
  }
}
```

When handling `kingdom.hello`, Kingdom:
1. Verifies that `session_id` matches the current session; if not, returns an error and disconnects
2. For the Worker role, verifies that `worker_id` exists in `state.json` → reconnects (sets `mcp_connected = true`)
3. For the Worker role, verifies that `worker_id` does **not** exist in `state.json` → rejects with an error (new workers must first be registered by the manager via `worker.create`; implicit creation during handshake is not allowed)
4. Binds connection → worker_id; any existing connection for the same `worker_id` is disconnected automatically
5. Returns `notification_mode`: M2/M3/M4 all return `"poll"`; switch to `"push"` after M8 implements push delivery
6. Returns `queued_notifications`: queued `kingdom.event` messages accumulated during disconnect (implemented in M8; M2 returns an empty array)

**Tool dispatch (`src/mcp/dispatcher.rs`):**

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn allowed_roles(&self) -> Vec<WorkerRole>;
    async fn call(&self, params: serde_json::Value, caller: &ConnectedClient) -> Result<serde_json::Value, McpError>;
}

pub struct Dispatcher {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl Dispatcher {
    pub fn register(&mut self, tool: Box<dyn Tool>);
    pub fn tools_for_role(&self, role: &WorkerRole) -> Vec<&str>;
    pub async fn dispatch(&self, request: McpRequest, caller: &ConnectedClient) -> McpResponse;
}
```

**Error handling (`src/mcp/error.rs`):**

```rust
pub enum McpError {
    Unauthorized { tool: String, role: String },
    JobNotFound(String),
    WorkerNotFound(String),
    InvalidState { message: String },
    ValidationFailed { field: String, reason: String },
    Internal(String),
}
```

**Replay deduplication (`src/mcp/replay.rs`):**

```rust
pub struct RecentCalls {
    // key: (worker_id, jsonrpc_id), meaning the JSON-RPC message's "id" field, TTL 5 minutes
    // do not introduce an extra call_id field; the provider guarantees ids are unique within the same connection
    calls: HashMap<(String, String), (Instant, serde_json::Value)>,
}

impl RecentCalls {
    pub fn check(&self, worker_id: &str, jsonrpc_id: &str) -> Option<&serde_json::Value>;
    pub fn insert(&mut self, worker_id: &str, jsonrpc_id: &str, result: serde_json::Value);
    pub fn evict_expired(&mut self);  // clean up expired entries during each insert
}
```

When a provider reconnects, it resends any tool calls that have not yet received a response using the same JSON-RPC id. If Kingdom hits `RecentCalls`, it returns the cached result directly and does not execute the tool again. No extra fields are needed.

**Skeleton tool registration:**
- Register all manager and worker tools from INTERFACES.md
- Each tool returns `serde_json::Value::Null` or an empty structure
- Permission checks must be implemented: if a worker calls a manager-only tool, return `McpError::Unauthorized`

**Socket paths:**
```
/tmp/kingdom/{workspace_hash}.sock        # MCP socket (provider only)
/tmp/kingdom/{workspace_hash}-cli.sock    # CLI socket (for `kingdom log` / `doctor` / `clean`, etc.)
```

**CLI Socket (`src/mcp/cli_server.rs`):**

Completely separate from the MCP socket. CLI commands (`kingdom log`, `kingdom doctor`, `kingdom restart`, etc.) use this socket to send JSON requests to the daemon and receive JSON responses. It does not use JSON-RPC; it is a simple request/response protocol:

```json
// CLI → Daemon
{ "cmd": "status" }
{ "cmd": "log", "limit": 50 }
{ "cmd": "shutdown" }
{ "cmd": "ready" }          // used by kingdom up to poll whether the daemon is ready

// Daemon → CLI
{ "ok": true, "data": { ... } }
{ "ok": false, "error": "daemon not initialized" }
```

The daemon listens on both sockets at startup. `kingdom up` polls the CLI socket using the `ready` command every 100ms with a 15-second timeout to confirm the daemon is ready, then starts the manager provider.

**Outbound push (`src/mcp/push.rs`):**

M2 implements the worker-directed push infrastructure (`kingdom.cancel_job`, `kingdom.checkpoint_request` are needed in M3/M6). Manager event push (`kingdom.event`) is completed in M8.

```rust
pub struct PushRegistry {
    // worker_id → write half of the MCP connection
    connections: HashMap<String, Arc<Mutex<WriteHalf<UnixStream>>>>,
}

impl PushRegistry {
    pub async fn push(&self, worker_id: &str, notification: serde_json::Value) -> Result<()>;
    pub fn register(&mut self, worker_id: &str, write: WriteHalf<UnixStream>);
    pub fn deregister(&mut self, worker_id: &str);
}
```

### Not Implemented

- Actual business logic for tools (implemented in M3/M4)
- Kingdom → Manager `kingdom.event` push (M8, manager notifications only, not worker commands)
- Heartbeat tracking
- Process management

### Acceptance Criteria

- [ ] `cargo test` passes
- [ ] A test MCP client can connect to the socket, send `kingdom.hello`, and complete the handshake
- [ ] The handshake response includes `tools`, `notification_mode`, and `queued_notifications` (empty array)
- [ ] A handshake with a mismatched `session_id` returns an error and disconnects
- [ ] A handshake with a `worker_id` that does not exist in `state.json` returns an error (implicit creation is not allowed)
- [ ] After the handshake, the correct tool list is returned (different for manager and worker)
- [ ] Worker calls to manager-only tools return `Unauthorized`
- [ ] All tool names defined in INTERFACES.md are registered, even if only as skeletons
- [ ] Replay deduplication: a second call with the same `(worker_id, jsonrpc_id)` returns the cached result and does not trigger business logic
- [ ] `RecentCalls` entries are cleaned up after TTL expiration
- [ ] Socket files are removed after abnormal server exit
- [ ] Multiple clients can connect simultaneously under the same `session_id`

### Reference Docs

- `INTERFACES.md` §MCP Tool Signatures
- `INTERFACES.md` §MCP Protocol Definition (handshake, reconnect, replay deduplication)
- `ARCHITECTURE.md` §MCP Socket Management
- `ARCHITECTURE.md` §Two Independent Channels
- `MCP_CONTRACT.md` §Design Principles

---

## M3: Manager Toolset

### Goal

Implement the business logic for all manager MCP tools. Tool calls read and write `Storage`, update `Session` state, and write to the action log.
This milestone has no real worker processes; all worker state is written directly into `state.json` by tests.

### Scope

**Implement the following tools (`src/mcp/tools/manager/`):**

`workspace.status`
- Read the current Session from Storage
- Return `WorkspaceStatus`

`workspace.log`
- Read the most recent N entries from `action.jsonl` (default 50)
- Support the `limit` parameter

`workspace.note`
- Create a `WorkspaceNote` and write it into Session
- Scope parsing: `"global"` / path string / `"job:{job_id}"`
- Persist to `state.json`

`workspace.notes`
- Return all notes, sorted by scope priority (job > directory > global)

`job.create`
- Generate a job_id (`job_{seq:03}`; `seq` is read from `state.json` and incremented)
- If `git.strategy == Branch`: create the `kingdom/{job_id}` branch and record `branch_start_commit = git rev-parse HEAD`
- If `git.strategy == Commit`: do not create a branch, but still record `branch_start_commit = git rev-parse HEAD` (used for changed_files calculation)
- If `git.strategy == None`: `branch_start_commit = None`
- Check that every `job_id` in `depends_on` exists; if not, return an error
- If all dependencies are completed → status = Pending; otherwise status = Waiting
- If `worker_id` is provided: automatically call assign logic
- Write to `state.json` + action log

`job.status`
- Return `JobStatusResponse`

`job.result`
- If the job is not completed, return `InvalidState`
- Read from `.kingdom/jobs/{job_id}/result.json`

`job.cancel`
- Check whether any other jobs depend on this one (waiting state)
- If yes, write a cascade warning to the action log (do not auto-cancel; the manager decides)
- If a worker is running: change job status to Cancelling, and Kingdom sends a `kingdom.cancel_job` notification to the worker (Phase 1 graceful stop)
- If no worker is running (Pending/Waiting/Paused): change job status directly to **Cancelled** (not Failed)
- Write to the action log

`kingdom.cancel_job` notification format (Kingdom → Worker):
```json
{
  "jsonrpc": "2.0",
  "method": "kingdom.cancel_job",
  "params": { "job_id": "job_001", "reason": "manager_cancelled" }
}
```

After receiving it, the worker calls `job.cancelled()` to confirm, and Kingdom changes the job to `Cancelled` (after a 30-second timeout it force-kills the worker and also marks the job as `Cancelled`).

`job.keep_waiting`
- Change job status back to Waiting (used when the manager chooses to keep waiting after a dependency failure)

`job.update`
- Update the intent; if the job is in Waiting/Paused/Failed, change it to Pending

`job.respond`
- Find the corresponding pending request, store the answer
- Mark the request as answered
- Write to the action log

`worker.assign`
- Verify that the worker exists and is in **Idle** state (a worker can only run one job at a time; Running returns `InvalidState { code: "WORKER_BUSY" }`)
- Verify that the job exists and is in **Pending** state (Waiting/Running/Completed/Failed/Cancelled return the corresponding error code)
- Update job.worker_id, change job.status → Running
- Update worker.job_id, change worker.status → Running
- Write to the action log

Valid state constraints (full table, see INTERFACES.md §MCP protocol definition):
| worker status | result |
|---|---|
| Idle | ✓ allowed |
| Running | ✗ `WORKER_BUSY` |
| Terminated | ✗ `WORKER_NOT_FOUND` |
| Does not exist | ✗ `WORKER_NOT_FOUND` |

`worker.release`
- Verify that the worker is Idle; otherwise return `InvalidState`
- Update worker.status → Terminated (actual process termination is implemented in M5)
- Write to the action log

`worker.grant` / `worker.revoke`
- Update the worker.permissions list
- Notify the dispatcher to refresh that worker’s available tool set

`failover.confirm` / `failover.cancel`
- Store the user’s decision for a pending failover
- The actual switching logic is implemented in M7

**Action log recording rules:**
- Write one `ActionLogEntry` after every successful tool call
- actor = manager’s worker_id
- action = tool name (for example `"job.create"`)
- params = call parameters (sanitized: do not record answer content, only the request_id)

### Not Implemented

- Actual process start/termination (`worker.create` / `worker.release` only update state)
- `worker.create` (process startup is in M5)
- `worker.swap` (failover is in M7)
- Kingdom → Manager notification push
- tmux operations

### Acceptance Criteria

- [ ] `cargo test` passes
- [ ] `job.create` → `job.status` returns the correct status
- [ ] When `job.create` has dependencies, missing `depends_on` jobs return an error
- [ ] When all dependencies are completed, `job.create` sets status = Pending; otherwise Waiting
- [ ] When `worker_id` is passed to `job.create`, assignment happens automatically
- [ ] `workspace.note` persists and `workspace.notes` can read it back
- [ ] `worker.assign` returns `InvalidState { code: "WORKER_BUSY" }` for a Running worker
- [ ] `worker.assign` returns the corresponding error for any job other than Pending
- [ ] `worker.release` returns InvalidState for a Running worker
- [ ] After `worker.grant`, the dispatcher’s tool list includes the newly granted tool
- [ ] Every tool call is written to the action log, and `workspace.log` can read it
- [ ] `job.cancel` with a worker changes the job to Cancelling and sends `kingdom.cancel_job`
- [ ] `job.cancel` without a worker (Pending/Waiting) changes the job directly to Cancelled (not Failed)
- [ ] `job.cancel` with cascading jobs writes a cascade warning to the log
- [ ] After `worker.create` registers a worker, receiving the `kingdom.hello` handshake triggers `ManagerNotification::WorkerReady`

### Reference Docs

- `INTERFACES.md` §MCP Tool Signatures
- `INTERFACES.md` §Return Value Structures
- `MCP_CONTRACT.md` §Manager Toolset
- `CORE_MODEL.md` §Job Dependencies
- `OPEN_QUESTIONS.md` Q30 (job cancellation cascade)
- `OPEN_QUESTIONS.md` Q44 (job.create + assign merge)

---

## M4: Worker Toolset

### Goal

Implement the business logic for all worker MCP tools. This includes task reporting, checkpoints, context tracking, and file reads.
This milestone depends on M3 being complete (job state is maintained by M3 tools).

### Scope

**Implement the following tools (`src/mcp/tools/worker/`):**

`job.progress`
- Update `worker.last_progress` timestamp
- Write to the action log (including note content)
- Send a notification to the manager (this milestone only writes to the queue; push is in M8)

`job.complete`
- **Idempotent**: if the job is already completed, calling it again returns Ok without error
- Validate that `result_summary` is non-empty and at least 20 characters; otherwise return ValidationFailed
- Update job.status → Completed and write `result.json`
- Attach `changed_files` (source depends on git strategy; see INTERFACES.md §changed_files calculation rules):
  - `Branch`: `git diff --name-only {job.branch_start_commit}..HEAD` (`branch_start_commit` is the commit recorded when the branch was created)
  - `Commit`: `git diff --name-only {job.branch_start_commit}..HEAD` (`branch_start_commit` is the `git rev-parse HEAD` value recorded when the job started, same recording style as Branch, except no branch is created)
  - `None`: empty list `[]` (Kingdom cannot calculate it; the worker can describe changes in `result_summary`, but the `changed_files` field is always empty)
- worker.status → Idle
- Write to the action log
- Enqueue manager notification: `ManagerNotification::JobCompleted`
- Check whether any waiting jobs now have all dependencies satisfied; if so, change them to Pending and enqueue `JobUnblocked`

`job.fail`
- Update job.status → Failed
- worker.status → Idle
- Write to the action log
- Enqueue manager notification: `ManagerNotification::JobFailed`

`job.cancelled`
- Confirm graceful stop completion (response for the two-stage `job.cancel` shutdown)
- job.status → **Cancelled** (not Failed; Cancelled means user-initiated cancellation, with distinct semantics)
- worker.status → Idle
- Write to the action log

`job.checkpoint`
- Validate that all five fields of `CheckpointSummary` are non-empty and each has at least 20 characters; otherwise return ValidationFailed
- Save `CheckpointContent` to `.kingdom/jobs/{job_id}/checkpoints/{id}.json`
- If `git.strategy != None`: run `git add -A && git commit -m "[kingdom checkpoint] {job_id}: {first 50 chars of done}"`
- Update the job.checkpoints list
- Write to the action log

`job.request`
- Generate request_id (`req_{seq:03}`)
- Store it in `Session.pending_requests`
- Enqueue manager notification: `ManagerNotification::WorkerRequest`
- Return request_id

`job.request_status`
- Look up the request in `pending_requests`
- Return `RequestStatus { answered: bool, answer: Option<String> }`

`job.status` (worker version)
- Only allows querying the worker’s currently assigned job; querying any other job returns Unauthorized

`context.ping`
- Update `worker.context_usage_pct` and `worker.token_count`
- Update `worker.last_heartbeat`
- Trigger `HealthEvent` based on usage_pct:
  - ≥0.50 → `ContextThreshold { urgency: Normal }`
  - ≥0.70 → `ContextThreshold { urgency: High }`
  - ≥0.85 → `ContextThreshold { urgency: Critical }`
  - ≥0.90 → add to failover candidate queue (handled in M7)
- Write into the HealthEvent queue (consumed in M6)

`context.checkpoint_defer`
- Respond to Kingdom’s `kingdom.checkpoint_request` notification (the worker proactively calls this tool to tell Kingdom it cannot checkpoint right now)
- If urgency = Critical, return ValidationFailed (no deferral allowed; see INTERFACES.md §context.checkpoint_request protocol)
- If urgency = Normal/High, record the deferral request and update the next trigger time (Normal: +60s, High: +15s)
- Write to the action log

`file.read`
- Read a file under `workspace_path`
- `lines` parameter format: `"100-300"`, parsed as a line range
- `symbol` parameter: M4 does not perform AST parsing; when `symbol` is present, fall back to full-file reading, with behavior:
  1. Add this header to the returned content: `# [symbol lookup not supported in M4, falling back to full file read]`
  2. Write an action log entry: `{ action: "file.read.symbol_fallback", params: { path, symbol } }`
  3. Do not return an error (M10 may extend this to real AST parsing)
- Default: first 200 lines
- Path must be within `workspace_path` (to prevent path traversal)

`workspace.tree`
- Execute `find {path} -maxdepth 3 -not -path '*/\.*'`
- Return the directory tree string
- Default path = `workspace_path`

`git.log`
- Execute `git -C {workspace_path} log --oneline -{n}`
- Parse into `Vec<GitLogEntry>`

`git.diff`
- Execute `git -C {workspace_path} diff {path}`
- Return a unified diff string

**Extended tools (permission required):**

`subtask.create`
- Requires `Permission::SubtaskCreate`, otherwise Unauthorized
- Calls the `job.create` logic and forcibly appends `depends_on=[caller’s job_id]`
- Creator is recorded as the caller’s worker_id
- Enqueue notification: `ManagerNotification::SubtaskCreated`

`worker.notify`
- Requires `Permission::WorkerNotify`, otherwise Unauthorized
- Enqueue the message in the target worker’s notification queue (M8 push)

### Not Implemented

- Actual handling of HealthEvent (in M6)
- Actual ManagerNotification push (in M8)
- AST parsing for the `symbol` parameter (may be extended in M10)

### Acceptance Criteria

- [ ] `cargo test` passes
- [ ] `job.complete` is idempotent: calling it twice in a row returns Ok and leaves the job unchanged
- [ ] `job.complete` returns ValidationFailed when `result_summary` is shorter than 20 characters
- [ ] `job.complete` in Branch strategy calculates `changed_files` from `branch_start_commit` (using a temporary git repo in tests)
- [ ] `job.complete` in Commit strategy also calculates `changed_files` from `branch_start_commit` (no branch is created; only the starting commit is recorded)
- [ ] `job.complete` in None strategy produces an empty `changed_files` list
- [ ] After `job.complete`, waiting jobs that depend on it become Pending
- [ ] `job.checkpoint` returns ValidationFailed when any of the five fields is shorter than 20 characters
- [ ] `job.checkpoint` creates a commit in git mode (using a temporary git repo in tests)
- [ ] `context.ping` with usage_pct=0.72 triggers a HealthEvent (urgency=High)
- [ ] `context.checkpoint_defer` with urgency=Critical returns ValidationFailed
- [ ] When `job.request` has `blocking=true`, the HTTP/JSON-RPC connection remains open (long polling) until `job.respond` is called, then returns a response
- [ ] When `job.request` has `blocking=false`, it returns the request_id immediately without waiting for an answer
- [ ] A path traversal attempt in `file.read` (`../../../etc/passwd`) returns an error
- [ ] After calling `job.cancelled`, the job.status is Cancelled (not Failed)
- [ ] `subtask.create` without permission returns Unauthorized
- [ ] `subtask.create` with permission includes the caller’s job_id in the job’s depends_on list

### Reference Docs

- `INTERFACES.md` §MCP Tool Signatures
- `MCP_CONTRACT.md` §Worker Default Toolset
- `MCP_CONTRACT.md` §Worker Initial Context
- `CONTEXT_MANAGEMENT.md` §Layer 1: Worker Autonomous Checkpoint
- `OPEN_QUESTIONS.md` Q16 (job.request loop)
- `OPEN_QUESTIONS.md` Q21 (git + checkpoint auto-commit)

---

## M5: Process Manager

### Goal

Implement provider process startup, tracking, and termination. Implement the core flow for `kingdom up` and `kingdom down`.
After this milestone is complete, a Claude/Codex process can actually be started in tmux and connected to the Kingdom MCP.

### Scope

**Provider discovery (`src/process/discovery.rs`):**

```rust
pub struct ProviderDiscovery;

impl ProviderDiscovery {
    // detect installed providers and return the available list
    pub fn detect(config: &KingdomConfig) -> Vec<DetectedProvider>;
    // check whether the binary for a single provider is available
    pub fn check(provider: &str, config: &KingdomConfig) -> Option<PathBuf>;
    // check whether the corresponding API key environment variable is set
    pub fn check_api_key(provider: &str) -> bool;
}

pub struct DetectedProvider {
    pub name: String,
    pub binary: PathBuf,
    pub api_key_set: bool,
}
```

Built-in API key environment variable mapping:
- `claude` → `ANTHROPIC_API_KEY`
- `codex` → `OPENAI_API_KEY`
- `gemini` → `GEMINI_API_KEY`

**Provider startup (`src/process/launcher.rs`):**

```rust
pub struct ProcessLauncher {
    workspace_path: PathBuf,
    config: KingdomConfig,
}

impl ProcessLauncher {
    // start provider process, returning PID and tmux pane_id
    pub async fn launch(
        &self,
        provider: &str,
        role: WorkerRole,
        worker_id: &str,
        job_id: Option<&str>,
        initial_context: Option<&str>,
    ) -> Result<LaunchResult>;

    // terminate process (graceful: SIGTERM + 5s → SIGKILL)
    pub async fn terminate(&self, pid: u32, graceful: bool) -> Result<()>;
}

pub struct LaunchResult {
    pub pid: u32,
    pub pane_id: String,
}
```

**Provider Adapter (`src/process/adapter.rs`):**

Different CLIs vary in startup arguments, working directory, exit codes, and first-interaction behavior, so the adapter provides a unified fallback; the launcher does not invoke raw templates directly.

```rust
pub trait ProviderAdapter: Send + Sync {
    // generate the full startup command (final args after placeholder replacement)
    fn build_args(&self, mcp_config_path: &Path, role: WorkerRole) -> Vec<String>;
    // provider process working directory (None = inherit Kingdom working directory)
    fn working_dir(&self, workspace_path: &Path) -> Option<PathBuf>;
    // determine whether the exit code is a "clean exit" (does not trigger failover)
    fn is_clean_exit(&self, code: i32) -> bool;
    // wait time before MCP connection becomes ready (first startup may have initialization delays)
    fn connection_grace_period(&self) -> Duration;
}

pub struct ClaudeAdapter;
pub struct CodexAdapter;
pub struct GeminiAdapter;
pub struct CustomAdapter { args_template: Vec<String> }
```

**Known differences among the three built-in adapters:**

| Property | claude | codex | gemini |
|---|---|---|---|
| MCP config arg | `--mcp-config {path}` | `--mcp-config {path}` | to be confirmed |
| working directory | inherit | inherit | inherit |
| clean exit code | 0 | 0 | 0 |
| connection wait | 3s | 5s | 5s |
| special first-start behavior | may show auth prompt | may require login | to be confirmed |

**Note:** Fields marked "to be confirmed" in the table will be filled in with real values during M5 integration. Before integration, use conservative defaults (5s wait, treat any exit code as a crash). If adapter behavior differs from the documentation during integration, the implementation takes precedence, and the table should be updated afterward.

**Startup flow:**
1. Select the appropriate adapter based on the `provider` name
2. Generate the MCP config file (based on the `worker.json` template, injecting `job_id`)
3. Run `tmux split-window -h -P -F "#{pane_id}"` to obtain `pane_id`
4. Execute the full command returned by `adapter.build_args()` inside the pane, using the working directory from `adapter.working_dir()`
5. Inject a prompt line at the top of the pane: `[Kingdom] Direct input here will not be recorded; use only for emergency intervention`
6. Record the PID (obtained via `tmux display-message -p "#{pane_pid}"`)
7. Wait for `adapter.connection_grace_period()`; if the handshake still has not happened by timeout, treat startup as failed

**Worker.create tool implementation (completes the M3 skeleton):**

```rust
// actual startup logic
pub async fn worker_create(provider: &str) -> Result<WorkerId> {
    // 1. Check that the provider is installed
    // 2. Generate worker_id (w{seq})
    // 3. Choose pane placement (main window ≤3 workers use split-window; otherwise create a new window)
    // 4. Call ProcessLauncher::launch
    // 5. Update Session and write state.json
    // 6. Write to the action log
}
```

**Pane layout strategy:**
- First worker in the main window: `tmux split-window -h` (vertical split)
- Second and third workers in the main window: `tmux split-window -v` (horizontal split)
- More than 3 workers: `tmux new-window -n "kingdom:w{n}"`

**Daemon PID file:**

When the daemon starts, it writes its own PID to `.kingdom/daemon.pid` as a plain number followed by a newline. The Kingdom CLI (`kingdom down` / `kingdom restart`) reads this file to obtain the daemon PID:
```rust
// when daemon main() starts
let pid = std::process::id();
std::fs::write(storage.root.join("daemon.pid"), format!("{}\n", pid))?;
```

**Watchdog (`crates/kingdom-watchdog/src/main.rs`):**

Implement according to the design in the technical choices section. Additional requirements:
- When it receives `SIGTERM`, it sends `SIGTERM` to the daemon, waits for the daemon to exit, then exits itself
- The watchdog PID is written to `.kingdom/watchdog.pid`

**Kingdom up (`src/cli/up.rs`):**

```
1. Check whether tmux is installed
2. Check git (if missing, warn and ask whether to continue with strategy=none)
3. Detect an existing session (check socket + state.json):
   - daemon + session both exist → prompt kingdom attach, then exit
   - daemon exists, session lost → rebuild the tmux session, jump to step 8
   - brand new startup → continue
4. Storage::init (create .kingdom/ directory + .gitignore)
5. ProviderDiscovery::detect, print available providers and API key status
6. If the manager provider is unavailable → error and exit
7. Generate MCP config (manager.json + worker.json)
8. Start the Kingdom MCP server (background daemon)
9. Ask for the default manager provider (only list available ones)
10. Create the tmux session (session_name comes from config.toml)
11. Start the manager in pane-0 and inject the manager system prompt
12. Wait for the manager MCP connection (15s timeout)
13. Print: ✓ startup complete
```

**Manager System Prompt Injection:**

When the manager starts, Kingdom automatically constructs an initial context message from the result of the MCP tool `workspace.status()`, in this format:

```
You are Kingdom's manager.
Current workspace: {workspace_path}
Available worker providers: {available_providers}
Current state: {workspace.status snapshot}

Your responsibilities: analyze user intent, split tasks, dispatch to workers, review results.
Interact with Kingdom through MCP tools; do not operate on the file system directly.

{KINGDOM.md content (if present)}
```

**Kingdom down (`src/cli/down.rs`):**

```
1. Check whether there are any running jobs
2. If yes → ask: [wait for completion] [pause and exit] [force exit]
   - wait for completion: poll until all jobs finish
   - pause and exit: send each running worker a checkpoint request (10s window), then force-exit on timeout
   - force exit: SIGKILL directly
3. If no → exit directly
4. Terminate in order: worker → manager → MCP server → watchdog
5. Clean up socket files
```

**Config hot reload (`src/config/watcher.rs`):**

After daemon startup, begin a background task that checks the modification time of `.kingdom/config.toml` every 5 seconds (`metadata().modified()`). If it changes, reload it and send `SIGHUP` to self to trigger the handler:

```rust
pub async fn config_watcher(config_path: PathBuf, config: Arc<RwLock<KingdomConfig>>) {
    let mut last_modified = std::time::SystemTime::UNIX_EPOCH;
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if let Ok(meta) = std::fs::metadata(&config_path) {
            if let Ok(modified) = meta.modified() {
                if modified > last_modified {
                    last_modified = modified;
                    match KingdomConfig::load(&config_path) {
                        Ok(new_cfg) => {
                            *config.write().await = new_cfg;
                            tracing::info!("config.toml reloaded");
                        }
                        Err(e) => tracing::warn!("config reload failed: {}", e),
                    }
                }
            }
        }
    }
}
```

**Note:** The following config fields take effect immediately after modification: `idle.timeout_minutes`, `health.*`, `notifications.*`. The following fields require `kingdom restart` to take effect (because they affect already-running processes): `tmux.session_name`, `providers.*`.

**KINGDOM.md template (`kingdom up` first run):**

If the workspace root has no `KINGDOM.md` and no `KINGDOM.md.example`, `kingdom up` asks during initialization:

```
KINGDOM.md not found. Generate a template? [Y/n]
```

If Y, generate a template based on the language/framework detected in the workspace:

```markdown
# Kingdom Working Constraints

## Coding Standards
- Language: {detected result, e.g. Rust / TypeScript / Python}
- Forbidden: {e.g. unwrap(), any, print debugging}

## Architecture Constraints
- (Describe architecture decisions that must not be changed here)

## Style Preferences
- (Describe the code style that AI should follow here)
```

Language detection: read the root `Cargo.toml` / `package.json` / `pyproject.toml` / `go.mod`, and use the first match.

**Idle timeout (`src/process/idle_monitor.rs`):**

```rust
// background task, checks once per minute
pub async fn idle_monitor(session: Arc<Mutex<Session>>) {
    // find all workers with status=Idle and idle time longer than config.idle.timeout_minutes
    // call ProcessLauncher::terminate
    // update worker.status → Terminated
    // write to the action log
}
```

### Not Implemented

- Health monitoring (M6)
- Failover (M7)
- tmux status bar updates (M8)
- `kingdom log / doctor / clean` (M9)

### Acceptance Criteria

- [ ] `cargo test` passes (process-related tests may use mocks)
- [ ] `kingdom up` succeeds in a new directory, creating `.kingdom/` and a tmux session
- [ ] `kingdom up` errors out when the manager provider is unavailable
- [ ] `kingdom up` prompts to attach when it detects an existing session
- [ ] `kingdom up` warns and asks when run in a non-git directory
- [ ] `worker.create` starts a process and records its PID
- [ ] The 4th worker started by `worker.create` launches in a new tmux window
- [ ] `kingdom down` exits directly when there are no running jobs
- [ ] `kingdom down` shows three options when there are running jobs
- [ ] After idle timeout, the worker process is terminated and its status becomes Terminated
- [ ] `kingdom down --force` skips prompts and terminates all processes immediately
- [ ] After daemon startup, `.kingdom/daemon.pid` contains the current daemon PID
- [ ] After `config.toml` changes, the daemon reloads automatically within 5 seconds without restart (`idle.timeout_minutes` changes take effect immediately)
- [ ] `kingdom up` asks to generate a template when `KINGDOM.md` is not found
- [ ] `.kingdom/watchdog.pid` exists after the watchdog starts
- [ ] After killing the daemon process, the watchdog restarts it within 1 second
- [ ] After `kingdom down`, the watchdog exits normally and does not restart in a loop

### Reference Docs

- `ARCHITECTURE.md` §Provider Discovery
- `ARCHITECTURE.md` §Provider Startup Flow
- `ARCHITECTURE.md` §Kingdom Startup Order
- `ARCHITECTURE.md` §MCP Config Structure
- `ARCHITECTURE.md` §Manager Initial Prompt
- `UX.md` §Tmux Layout
- `UX.md` §Shutdown UX
- `OPEN_QUESTIONS.md` Q7 (Provider Discovery)
- `OPEN_QUESTIONS.md` Q10 (Bootstrap Details)
- `OPEN_QUESTIONS.md` Q11 (kingdom down)
- `OPEN_QUESTIONS.md` Q19 (concurrent worker limit)
- `OPEN_QUESTIONS.md` Q28 (worker idle reuse)
- `OPEN_QUESTIONS.md` Q35 (Provider startup args)
- `OPEN_QUESTIONS.md` Q41 (non-git directories)

---

## M6: Health Monitoring

### Goal

Implement continuous health monitoring for each provider in Kingdom. When anomalies are detected, trigger `HealthEvent`s as input for the M7 failover state machine.
This milestone is responsible only for **detection and event emission**, not for handling (handling happens in M7).

### Scope

**Health monitor (`src/health/monitor.rs`):**

```rust
pub struct HealthMonitor {
    session: Arc<Mutex<Session>>,
    config: HealthConfig,
    event_tx: mpsc::Sender<HealthEvent>,
}

impl HealthMonitor {
    pub fn new(
        session: Arc<Mutex<Session>>,
        config: HealthConfig,
        event_tx: mpsc::Sender<HealthEvent>,
    ) -> Self;

    // start the background monitoring loop
    pub async fn run(&self);
}
```

**Four monitoring dimensions:**

**1. Heartbeat monitoring**

```rust
// check once every heartbeat_interval_seconds
// compute the time since the last heartbeat for each connected worker
// if no heartbeat arrives for interval * timeout_count seconds → trigger an event
HealthEvent::HeartbeatMissed {
    worker_id,
    consecutive_count,  // number of consecutive missed responses
}
```

- Only monitor workers with `mcp_connected = true`
- `context.ping` handling (already implemented in M4) updates `last_heartbeat`; this module only reads the timestamp

**2. Process liveness monitoring**

```rust
// poll every 5 seconds (more frequent than heartbeats; process exits must be handled quickly)
// check whether the process corresponding to worker.pid is still alive
// detect via /proc/{pid}/status (Linux) or kill -0 {pid} (cross-platform)
// if the process does not exist → trigger an event
HealthEvent::ProcessExited {
    worker_id,
    exit_code,  // obtained from waitpid; if unavailable, use -1
}
```

- Process exits trigger **immediately**, without waiting for heartbeat timeout

**3. Context threshold monitoring**

```rust
// when context.ping updates worker.context_usage_pct (M4 already triggers HealthEvent)
// HealthMonitor consumes these events and decides whether to send checkpoint requests
// thresholds: 50% / 70% / 85% / 90%
// 90% goes directly into the failover candidate set; no checkpoint request is sent
```

**Checkpoint request flow (see INTERFACES.md §context.checkpoint_request protocol):**

```rust
// 1. Kingdom sends a `kingdom.checkpoint_request` notification to the worker (no id, one-way push):
//    { "method": "kingdom.checkpoint_request", "params": { "job_id": "job_001", "urgency": "Normal" } }
// 2. Wait (based on the urgency delay window):
//    - worker calls job.checkpoint()          → success, reset context threshold counter
//    - worker calls context.checkpoint_defer() → record deferment, update next trigger time
//    - if no response after the delay window → fallback: Kingdom generates a fallback checkpoint
// 3. Pending state for the same worker prevents duplicate sends (idempotent)
```

Deferral windows (corresponding to `CheckpointUrgency`):
- Normal (≥50%): up to 60 seconds
- High (≥70%): up to 15 seconds
- Critical (≥85%): no deferral allowed; `context.checkpoint_defer()` returns `ValidationFailed`

**4. Progress timeout monitoring**

```rust
// check once per minute
// find all workers with status=Running whose last_progress is older than progress_timeout_minutes
// trigger an event (not failover, just a warning)
HealthEvent::ProgressTimeout {
    worker_id,
    elapsed_minutes,
}
```

**Rate limit handling (`src/health/rate_limiter.rs`):**

```rust
// triggered when an MCP tool call returns a rate limit error
// does not enter failover; handled separately with exponential backoff retries
// backoff sequence: 5s → 15s → 30s → 60s (capped)
// if 3 retries still fail → trigger HealthEvent::ProcessExited (degrade to crash handling)

pub struct RateLimitHandler {
    retry_counts: HashMap<String, u32>,  // worker_id → retry count
}

impl RateLimitHandler {
    pub async fn handle(&mut self, worker_id: &str) -> RateLimitResult;
}

pub enum RateLimitResult {
    Retrying { wait_secs: u64 },
    Exhausted,  // degrade after 3 attempts
}
```

**Fallback checkpoint (`src/health/fallback_checkpoint.rs`):**

```rust
// used when the worker does not respond to a checkpoint request within the window
// contents: only git diff, no textual summary (all five fields empty but marked "[auto-generated, no summary]")
pub async fn generate_fallback_checkpoint(
    job_id: &str,
    workspace_path: &Path,
) -> CheckpointContent;
```

### Not Implemented

- Handling HealthEvent (failover triggering is in M7)
- Progress-timeout popup display (M8)
- Rate-limit status bar updates (M8)

### Acceptance Criteria

- [ ] `cargo test` passes (using mock processes and mock time)
- [ ] Heartbeat: a worker that has not updated `last_heartbeat` for 60 seconds triggers `HeartbeatMissed { consecutive_count: 2 }`
- [ ] Process: after killing a worker process, `ProcessExited` is triggered within 5 seconds
- [ ] Context 50%: Kingdom sends a `kingdom.checkpoint_request` notification with urgency=Normal
- [ ] Context 85%: Kingdom sends a `kingdom.checkpoint_request` notification with urgency=Critical
- [ ] Context 85% + worker calling `context.checkpoint_defer()`: returns ValidationFailed
- [ ] If the worker does not respond within 60s for Normal context, Kingdom generates a fallback checkpoint
- [ ] While the same worker is waiting for checkpoint, Kingdom does not resend `kingdom.checkpoint_request` (idempotent)
- [ ] If the worker receives `kingdom.checkpoint_request` and then `kingdom.cancel_job`, cancellation takes priority and checkpointing is not required first
- [ ] Progress timeout: 30 minutes without `job.progress` triggers `ProgressTimeout`
- [ ] Rate limit: after 3 consecutive attempts, degraded failover is triggered
- [ ] All `HealthEvent`s are written correctly to the action log

### Reference Docs

- `ARCHITECTURE.md` §Health Monitoring
- `INTERFACES.md` §context.checkpoint_request protocol (full `kingdom.checkpoint_request` notification specification)
- `CONTEXT_MANAGEMENT.md` §Layer 1: Worker Autonomous Checkpoint
- `FAILOVER.md` §Detection
- `OPEN_QUESTIONS.md` Q13 (failover false-positive protection)
- `OPEN_QUESTIONS.md` Q33 (progress timeout value)

---

## M7: Failover State Machine

### Goal

Implement the full failover flow: from `HealthEvent` trigger to a new provider taking over the work.
This includes circuit breaking, manual swap, rate-limit degradation, and manager failover.

### Scope

**Failover state machine (`src/failover/machine.rs`):**

```rust
pub struct FailoverMachine {
    session: Arc<Mutex<Session>>,
    config: FailoverConfig,
    health_rx: mpsc::Receiver<HealthEvent>,
    launcher: ProcessLauncher,
    tmux: TmuxController,        // use stub before M8
}

impl FailoverMachine {
    pub async fn run(&self);     // consume HealthEvent and drive the state machine
}
```

**Event priority and conflict handling (single-entry rule):**

All state changes must go through the `FailoverMachine::run` event loop. It is forbidden to modify worker/job state directly elsewhere. When multiple events arrive at once, handle them in this priority order:

| Priority | Event type | Description |
|---|---|---|
| 1 (highest) | `ProcessExited` | The process is dead, a deterministic event, handle immediately |
| 2 | `ManualSwap` | User-initiated, not counted toward circuit breaking |
| 3 | `ContextLimit` (≥90%) | Context is about to run out, switch immediately |
| 4 | `HeartbeatTimeout` | Heartbeat timeout, hung but not exited |
| 5 (lowest) | `ProgressTimeout` | Long period without reporting; warn only, do not switch |

**Explicit rules for conflict scenarios:**

| Scenario | Rule |
|---|---|
| `HeartbeatTimeout` and `ProgressTimeout` happen at the same time | Handle `HeartbeatTimeout` according to priority 4; drop `ProgressTimeout` |
| `ManualSwap` is received during rate-limit retry | Abort retries and execute `ManualSwap` immediately (priority 2 > rate-limit handler) |
| Provider crashes while job is `Cancelling` | Ignore failover, change the job directly to `Cancelled`, and do not start a new provider |
| ManualSwap occurs during a circuit-breaker window together with an automatic failover | `ManualSwap` is not constrained by circuit breaking (it does not count toward failure totals and is not subject to cooldown) |
| New provider times out while a second `HealthEvent` arrives | The current failover flow is not complete; queue the second event and wait |

When a worker is in failover flow (`failover_in_progress = true`), newly arrived `HealthEvent`s are cached in the queue. After the current flow completes (successfully or Paused), process the queue.

**Full failover flow:**

```
HealthEvent triggered
  ↓
1. Circuit-breaker check
   Same job failed ≥ 3 times within 10 minutes → mark job Paused, send ManagerNotification, stop
   Interval between two failovers < 30 seconds → wait for cooldown, then continue
  ↓
2. Determine whether the user manually stopped it (5-second buffer window)
   On ProcessExited, show popup (log instead of popup before M8):
   "Did you manually stop it? [Yes, pause the job] [No, trigger failover]"
   - No response in 5 seconds → treat as crash and continue
   - User selects "Yes" → mark job Paused and stop
  ↓
3. Prepare handoff brief
   - Take the most recent checkpoint content
   - Use git diff to compute `possibly_incomplete_files` (files last modified before the crash)
   - Construct `HandoffBrief`
  ↓
4. Confirm the failover (popup; log instead before M8)
   Show: failure reason + handoff brief summary + recommended provider
   [Confirm failover] [Choose another] [Cancel]
   - Cancel → mark job Paused and stop
  ↓
5. Start a new provider
   - Use the same pane (tmux respawn-pane)
   - Pass `HandoffBrief` as the initial context
  ↓
6. Wait for the new provider MCP connection (15-second timeout)
   Timeout → show warning, offer [Retry] [Choose another] [Pause]
  ↓
7. Write a HANDOFF separator to the pane (log instead before M8)
8. Update Session: new worker_id, job continues Running
9. Write to the action log
```

**Circuit breaker (`src/failover/circuit_breaker.rs`):**

```rust
pub struct CircuitBreaker {
    failure_records: HashMap<String, Vec<DateTime<Utc>>>,  // job_id → list of failure times
    config: FailoverConfig,
}

impl CircuitBreaker {
    pub fn record_failure(&mut self, job_id: &str) -> CircuitBreakerResult;
    pub fn check_cooldown(&self, worker_id: &str) -> Option<Duration>;
}

pub enum CircuitBreakerResult {
    Ok,
    Tripped,   // circuit breaker triggered
}
```

**Provider stability history (`src/failover/stability.rs`):**

Maintain crash/timeout counts for each provider during the current session in `state.json`, and incorporate them into failover recommendations:

```rust
pub struct ProviderStability {
    pub provider: String,
    pub crash_count: u32,        // number of failovers triggered by ProcessExited
    pub timeout_count: u32,      // number of failovers triggered by HeartbeatTimeout
    pub last_failure_at: Option<DateTime<Utc>>,
}
```

Update timing: after each failover completes, increment the corresponding count for the failed provider by 1 and write it to `state.json`.

**Recommendation logic extension:** Step 7 of the decision table sorts by stability: prefer the candidate with the fewest `crash_count + timeout_count`; if tied, sort by `claude > codex > gemini`.

**Recommended provider logic (`src/failover/recommender.rs`):**

```rust
pub fn recommend_provider(
    failed_provider: &str,
    available_providers: &[String],   // only detected available providers (not installed ones are excluded)
    failure_reason: &FailoverReason,
    session_failures: &[String],      // providers already failed in this failover chain, to avoid recommending them again
    manager_provider: &str,           // provider currently used by the manager; do not recommend it
    stability: &HashMap<String, ProviderStability>,  // stability history for this session
) -> Option<String>;
```

**Decision table (deterministic, unit-testable):**

| Step | Condition | Action |
|---|---|---|
| 1 | Candidate set = available_providers | Initial candidates |
| 2 | Exclude failed_provider | Remove from candidate set |
| 3 | Exclude all items in session_failures | Remove from candidate set |
| 4 | Exclude manager_provider | Remove from candidate set |
| 5 | Candidate set is empty | Return `None` |
| 6 | `failure_reason == ContextLimit` | If claude is in the candidate set → return claude |
| 7 | Other reasons | Return the first in priority order: claude > codex > gemini |

**Test cases (all must be implemented):**
- `recommend("codex", ["claude","codex","gemini"], Crash, [], "claude")` → `Some("gemini")` (claude is the manager and excluded)
- `recommend("codex", ["claude","codex"], ContextLimit, [], "gemini")` → `Some("claude")` (`ContextLimit` prioritizes claude)
- `recommend("claude", ["claude"], Crash, [], "n/a")` → `None` (candidate set becomes empty after exclusions)
- `recommend("codex", ["claude","codex"], Crash, ["claude"], "gemini")` → `None` (no candidates remain after removing `session_failures`)

**Manager failover special handling:**

```rust
// when the manager fails, all running workers pause receiving new tasks
// use the same failover flow, but the new manager's initial context is different:
pub fn build_manager_recovery_context(session: &Session) -> String {
    // includes: KINGDOM.md + workspace.notes + all job statuses + the most recent N action log entries
    // does not depend on conversation history (Q27)
}
```

**Manual Swap (`src/cli/swap.rs`):**

```
kingdom swap {worker_id} [provider]
  ↓
1. Verify that worker_id exists and is not the manager
2. Send the worker a checkpoint request (urgency=High, 10-second window)
3. Timeout → generate a fallback checkpoint using git diff
4. Show confirmation popup (print to stdout before M8)
5. Confirm → follow the standard failover flow, reason=Manual
6. Manual failover does not count toward circuit-breaker totals
```

**`job.cancel` two-stage shutdown (M3 supplement):**

```rust
// Phase 1: Kingdom sends an MCP notification to the worker:
//   {"jsonrpc":"2.0","method":"kingdom.cancel_job","params":{"job_id":"job_001","reason":"manager_cancelled"}}
// Wait 30 seconds
// Phase 2a: worker calls job.cancelled() → clean stop → job.status = Cancelled, stash changes with git
// Phase 2b: 30-second timeout → SIGKILL → job.status = Cancelled, stash changes as best effort with git
```

The final state for both phases is `Cancelled`, not `Failed`.

### Not Implemented

- tmux popup display (M8): use stdout logs in this milestone instead
- tmux HANDOFF separator (M8): use logs in this milestone instead
- status bar updates (M8)

### Acceptance Criteria

- [ ] `cargo test` passes
- [ ] `ProcessExited` → failover triggered → new provider starts (integration test uses mock provider)
- [ ] `HeartbeatTimeout` → failover triggered
- [ ] `ContextLimit` (90%) → failover triggered
- [ ] Three failures of the same job within 10 minutes → circuit breaker trips and job is marked Paused
- [ ] Two failovers less than 30 seconds apart → continue after cooldown
- [ ] If the new provider does not connect within 15 seconds, show a timeout warning
- [ ] `kingdom swap w1` sends a checkpoint request and falls back after a 10-second timeout
- [ ] Manual swap does not count toward circuit-breaker totals
- [ ] After manager failover, the new manager receives the full recovery context (including job statuses + notes)
- [ ] `job.cancel` graceful stop: worker completes `job.cancelled()` within 30 seconds
- [ ] `job.cancel` graceful stop timeout: git stash after SIGKILL
- [ ] All four `recommend_provider` decision-table test cases pass (see BUILD_PLAN §Recommended provider logic)
- [ ] When `HeartbeatTimeout` and `ProgressTimeout` arrive at the same time: only handle `HeartbeatTimeout`, drop `ProgressTimeout`
- [ ] During rate-limit retry, receiving `ManualSwap` aborts retries and immediately performs the switch
- [ ] When a job is in `Cancelling` and the provider crashes: the job changes to `Cancelled` and failover is not triggered
- [ ] ManualSwap is not constrained by circuit breaking (it can still run within the circuit-breaker window)
- [ ] When a second `HealthEvent` arrives during failover, queue it and process it after the current flow finishes
- [ ] After two provider crashes, `recommend_provider` prefers the candidate with fewer crashes
- [ ] The `provider_stability` field in `state.json` is updated correctly after each failover

### Reference Docs

- Full `FAILOVER.md`
- `CONTEXT_MANAGEMENT.md` §Handoff Brief During Switching
- `INTERFACES.md` §MCP Protocol Definition (`kingdom.cancel_job` notification, final state semantics)
- `OPEN_QUESTIONS.md` Q17 (file corruption during failover)
- `OPEN_QUESTIONS.md` Q21 (git strategy and failover)
- `OPEN_QUESTIONS.md` Q24 (provider reconnect)
- `OPEN_QUESTIONS.md` Q27 (manager context limit)
- `OPEN_QUESTIONS.md` Q8 (kingdom swap)

---

## M8: tmux Integration

### Goal

Implement all visible tmux-layer UX: status bar, confirmation popups, HANDOFF separators, session restore UX, and `ManagerNotification` push.
After this milestone is complete, the user-facing interaction experience is complete.

### Scope

**tmux controller (`src/tmux/controller.rs`):**

```rust
pub struct TmuxController {
    session_name: String,
}

impl TmuxController {
    // Status bar
    pub fn update_status_bar(&self, session: &Session) -> Result<()>;

    // Popup
    pub fn show_popup(&self, popup: &Popup) -> Result<PopupResult>;

    // Pane operations
    pub fn inject_line(&self, pane_id: &str, line: &str) -> Result<()>;
    pub fn respawn_pane(&self, pane_id: &str, command: &str) -> Result<()>;

    // Session management
    pub fn create_session(&self) -> Result<()>;
    pub fn session_exists(&self) -> bool;
    pub fn attach(&self) -> Result<()>;
}
```

**Status bar (`src/tmux/status_bar.rs`):**

Format: `[{provider}:{role_abbr}{status_icon}] ... {cost}  {time}`

```rust
pub fn render_status_bar(session: &Session) -> String;
```

Rules:
- Manager: `[Claude:mgr]`
- Worker running: `[Codex:w1]`
- Worker completed: `[Codex:w1✓]` (restores to no icon after 3 seconds)
- Worker attention: `[Gemini:w2⚠]`
- Worker failed: `[Codex:w3✗]`
- Worker failover: `[Codex:w1↻]`
- Worker rate limited: `[Codex:w1⏳]`
- Idle worker: `[idle]`

Update timing (triggered by events, not polling):
- worker status changes
- job completion/failure
- failover start/finish
- `context_usage_pct` changes (update every 10%)

```bash
tmux set-option -g status-right "{rendered_string}"
tmux refresh-client -S
```

**Popup (`src/tmux/popup.rs`):**

```rust
pub struct Popup {
    pub title: String,
    pub body: String,
    pub options: Vec<PopupOption>,
    pub timeout_secs: Option<u32>,
    pub default_on_timeout: Option<usize>,  // which option to choose after timeout (0-indexed)
}

pub struct PopupOption {
    pub label: String,
    pub key: char,               // shortcut key
}

pub enum PopupResult {
    Selected(usize),
    Timeout,
    Dismissed,
}

impl TmuxController {
    pub fn show_popup(&self, popup: &Popup) -> Result<PopupResult> {
        // generate a temporary shell script and display it via tmux display-popup
        // wait for user input and return the result
    }
}
```

**Popup timeout rules:**
- Failover confirmation popup: no timeout (wait for the user)
- Process-exit 5-second buffer popup: 5-second timeout, default to "No, trigger failover"
- Progress-timeout warning popup: no timeout

**tmux command fallback strategy:**

tmux commands may fail due to version differences, closed panes, lost sessions, and similar reasons. Fallback rules for each operation:

| Operation | Fallback on failure |
|---|---|
| `display-popup` (requires tmux ≥ 3.2) | fallback: send `send-keys` text to the manager pane, and write an action log requesting the user to handle it manually |
| `inject_line` text injection into pane | fallback: skip injection, write to action log, do not block the main flow |
| `update status bar` (`set-option`) | fallback: silently skip, retry on next attempt |
| `respawn-pane` (failover provider restart) | no fallback allowed; if it fails, report an error to the manager and mark the job Paused |
| `tmux new-window` (4+ workers) | fallback: report an error prompting the user to create a window manually, but do not block the `worker.create` state write |

**Version detection:** during `kingdom up`, record the tmux version in session state. Before `display-popup`, check the version; on older versions, go directly through the fallback path and do not try-and-fail first.

**Fallback text format for `display-popup` (inject into the manager pane):**
```
[Kingdom needs confirmation] worker Codex for job_001 crashed
  reason: HeartbeatTimeout
  action: failover.confirm("w1", "claude") switch | failover.cancel("w1") pause
```

**HANDOFF separator (`src/tmux/handoff.rs`):**

```rust
pub fn inject_handoff_separator(
    pane_id: &str,
    from_provider: &str,
    to_provider: &str,
    reason: &str,
    brief_summary: &str,
    tmux: &TmuxController,
) -> Result<()> {
    let line = format!(
        "────────────────────────────────────────────────\n\
         ⚡ HANDOFF  {} → {}                {}\n\
         reason: {}\n\
         passed on: {}\n\
         ────────────────────────────────────────────────",
        from_provider,
        to_provider,
        chrono::Utc::now().format("%H:%M:%S"),
        reason,
        brief_summary,
    );
    tmux.inject_line(pane_id, &line)
}
```

**Failover empty-window UX:**

```rust
// during the period between the old provider stopping and the new provider connecting, show a countdown in the pane:
// ⏳ Starting Claude... (3s)
// update every second; stop once the new provider connects
pub async fn show_startup_progress(
    pane_id: &str,
    provider: &str,
    tmux: &TmuxController,
    connected: watch::Receiver<bool>,
) -> Result<()>;
```

**Manager Notification push (`src/mcp/notifier.rs`):**

```rust
pub struct ManagerNotifier {
    manager_connection: Option<Arc<McpConnection>>,
    notification_mode: NotificationMode,
    pending_queue: VecDeque<ManagerNotification>,
}

impl ManagerNotifier {
    // Push mode: send a `kingdom.event` notification to the manager
    // method: "kingdom.event"
    // params: { "type": "<event_type>", "data": <event_data>, "text": "<formatted text>" }
    // the text field is a preformatted conversational message directly injected into provider context
    pub async fn push(&self, notification: &ManagerNotification) -> Result<()>;

    // when the manager disconnects: queue notifications; on reconnect during kingdom.hello, send them in batch via queued_notifications
    // queued_notifications has the same format as push (kingdom.event params array)
    pub async fn flush_queue(&self) -> Result<()>;

    // format a notification as a manager conversational message (Chinese, see MCP_CONTRACT.md §Notification format)
    pub fn format_notification(notification: &ManagerNotification) -> String;
}

// kingdom.event params example:
// {
//   "type": "job_completed",
//   "data": { "job_id": "job_001", "worker_id": "w1", "changed_files": [...] },
//   "text": "[Kingdom] job_001 completed\n  worker: w1 (Codex)\n  summary: ...\n  → call job.result(...)"
// }
```

Notification message example (injected into the manager conversation stream):

```
[Kingdom] job_001 completed
  worker: w1 (Codex)
  summary: Implemented login validation, modified 3 files
  changed: src/auth/LoginForm.tsx, src/auth/validation.ts
  → call job.result("job_001") to view the full result
```

**Session restore UX (supplement to M5 `kingdom up`):**

```
$ kingdom up

Detected unfinished work from the last session:

  job_001  implement login validation      [running → w1 paused]
  job_002  write frontend login request    [waiting → depends on job_001]
  job_003  add unit tests                  [completed ✓]

workspace.notes:
  · Use TypeScript; no any
  · Do not introduce new dependencies under src/auth/

Continue the previous work? [Y/n]
```

**kingdom attach (`src/cli/attach.rs`):**

```bash
tmux attach-session -t {session_name}
```

**System notifications (`src/notifications/system.rs`):**

```rust
pub fn send_notification(title: &str, body: &str, level: NotificationLevel) -> Result<()> {
    match level {
        NotificationLevel::Bell => print!("\x07"),  // terminal bell
        NotificationLevel::System => {
            // macOS: osascript -e 'display notification "{body}" with title "{title}"'
            // Linux: notify-send "{title}" "{body}"
            // fall back to Bell if the platform is unsupported
        }
        NotificationLevel::None => {}
    }
}
```

### Not Implemented

- M9 CLI commands (`log` / `doctor` / `clean` / `cost`)

### Acceptance Criteria

- [ ] `cargo test` passes (tmux commands use mocks)
- [ ] Status bar renders correctly (`render_status_bar` unit test)
- [ ] After worker status changes, the status bar updates within 1 second
- [ ] Failover popup shows the three options correctly
- [ ] The 5-second buffer popup automatically selects "No, trigger failover" after timeout
- [ ] HANDOFF separator has the correct format, including timestamp and reason
- [ ] Empty-window countdown updates every second
- [ ] Manager notifications are injected into the conversation stream with the correct format
- [ ] Notifications are queued when the manager disconnects and pushed after reconnect
- [ ] Session restore shows the correct job status summary
- [ ] Bell notifications trigger a terminal bell
- [ ] `kingdom attach` connects to the existing tmux session
- [ ] When `display-popup` fails (mocked older tmux version), it falls back to injecting text into the manager pane
- [ ] When `inject_line` fails, it is silently skipped and does not block the main flow; an action log entry is written
- [ ] When `update_status_bar` fails, it is silently skipped and does not panic
- [ ] When there are more than 4 workers, the status bar is simplified to `[w1:✓] [w2:⚡] [w3:✓] [+N]` so it does not overflow
- [ ] When failover is triggered by manager context exhaustion, the old manager pane receives a static marker line: `[Kingdom] This manager has been taken over by a new manager; please switch to the manager pane`; the status bar marks the old pane as `[manager:stale]`
- [ ] When Kingdom injects a job notification into the manager, it performs basic filtering on `result_summary`: truncate content longer than 2KB, detect and warn about suspicious instruction blocks with `system:` / `<system>` prefixes (write to the action log, but do not block injection)

### Reference Docs

- Full `UX.md`
- `FAILOVER.md` §Failover Empty-Window UX
- `MCP_CONTRACT.md` §Manager Conversation Model
- `MCP_CONTRACT.md` §Kingdom → Manager Push Events
- `OPEN_QUESTIONS.md` Q3 (popup timeout) — see the timeout rules in this milestone
- `OPEN_QUESTIONS.md` Q14 (typing directly into a worker pane)
- `OPEN_QUESTIONS.md` Q20 (notifications when leaving)
- `OPEN_QUESTIONS.md` Q25 (manager notification mechanism)

---

## M9: CLI Commands

### Goal

Implement all helper CLI commands: `log`, `doctor`, `clean`, `cost`, and `restart`.
These commands read `.kingdom/` state and do not modify runtime behavior.

### Scope

**`kingdom log` (`src/cli/log.rs`):**

Three mutually exclusive views:

```
kingdom log                        # default: job list
kingdom log --detail <job_id>      # full timeline for a single job
kingdom log --actions [--limit N]  # raw action stream
```

**Default view:**

```
job_003  ✓  add unit tests              completed  14:21  Codex(w3)  3m12s
job_002  ✓  write frontend login request completed  13:45  Gemini(w2) 8m04s
job_001  ✓  implement login validation  completed  12:58  Codex(w1)  22m31s
            ↳ failover: Codex→Claude 14:02 (context exhausted)
```

- Read from `state.json` (all job metadata lives in state.json; there is no separate meta.json)
- Sort by `created_at` in descending order
- Duration = `completed_at - created_at`
- Failover records are extracted from the action log

**`--detail` view:**

```
job_001  implement login validation
  created    12:58  by manager
  worker     Codex w1 (12:58 → 14:02)
  failover   14:02  context exhausted → Claude w4
  worker     Claude w4 (14:02 → 15:20)
  completed  15:20  3 files changed
  branch     kingdom/job_001

  checkpoints:
    13:15  [kingdom checkpoint] validation logic complete, form submission in progress
    13:19  [kingdom checkpoint] form submission complete, waiting to write tests
```

**`--actions` view:**

```
14:21  manager(w0)  job.complete   job_003
14:20  worker(w3)   job.progress   job_003  "all tests passed"
14:02  kingdom      failover       job_001  Codex→Claude
13:15  worker(w1)   job.checkpoint job_001
```

---

**`kingdom doctor` (`src/cli/doctor.rs`):**

Five layers of checks, output progressively. If Kingdom is not running, only system and configuration checks are performed.

```
Checking the Kingdom runtime environment...

[System Dependencies]
✓ tmux 3.3a
✓ git 2.42
✗ codex    not installed  → npm install -g @openai/codex
✓ claude   installed

[API Key]
✓ ANTHROPIC_API_KEY    set
✗ OPENAI_API_KEY       not set  → export OPENAI_API_KEY=sk-...

[Kingdom Daemon]
✓ daemon running  PID 12345  up for 2h34m
✓ MCP socket    /tmp/kingdom/a3f9c2.sock
✓ watchdog      running  PID 12346

[Current Session]
✓ manager    Claude   connected  context 23%
⚠ w1        Codex    heartbeat timeout 45s  → kingdom swap w1
✓ w2        Gemini   connected  context 41%

[Config Files]
✓ .kingdom/config.toml    valid
✓ KINGDOM.md              present
⚠ .kingdom/manager.json  MCP socket path stale  → kingdom restart
```

Implementation details:
- System dependencies: `which tmux`, `which git`, `which {provider}` + version
- API keys: check whether the environment variable exists (do not validate the actual key)
- Daemon: check socket file + PID liveness
- Session: read from state.json, check each worker’s `last_heartbeat`
- Config files: attempt to parse config.toml and verify that the socket path in manager.json matches the current one

---

**`kingdom clean` (`src/cli/clean.rs`):**

```
kingdom clean              # show preview and ask for confirmation
kingdom clean --dry-run    # only show, do not execute
kingdom clean --all        # ignore the time limit and clean everything that can be cleaned
```

Cleanup targets (per ARCHITECTURE.md retention policy):
- Intermediate checkpoints for completed jobs (keep the latest one, delete the rest older than 7 days)
- Final results for completed jobs (archive to `.kingdom/archive/` after 90 days)
- Old action log entries (compress entries older than 30 days into summary lines: `[compressed] {date}: {n} actions`)

Output format:

```
The following will be cleaned:

  Intermediate checkpoints for completed jobs (>7 days)
    job_001  3 checkpoints  · 2.1 MB  2026-03-15
    job_002  5 checkpoints  · 4.8 MB  2026-03-18

  Archive completed job results (>90 days)
    job_047  · 1.2 MB  2025-12-10

  Compress old action logs (>30 days)
    2026-02-01 ~ 2026-03-05  · 18 MB → about 0.5 MB

Total freed: about 26 MB

[Confirm cleanup]  [Cancel]
```

---

**`kingdom cost` (`src/cli/cost.rs`):**

```
Today’s cost: $0.34
  Claude   128k tokens   $0.19  ████████░░
  Codex     89k tokens   $0.11  █████░░░░░
  Gemini    45k tokens   $0.04  ██░░░░░░░░

This week: $2.17  This month: $8.43

Most expensive job: job_003 (implement login validation) $0.18
```

Built-in unit prices (can be overridden in config.toml):

```toml
[cost]
claude_input_per_1m = 3.00      # USD
claude_output_per_1m = 15.00
codex_input_per_1m = 2.50
codex_output_per_1m = 10.00
gemini_input_per_1m = 0.075
gemini_output_per_1m = 0.30
```

Data source: token_count deltas recorded in each `context.ping` entry in the action log.

**Data definition conventions (shared by the three commands; must be followed when writing to the action log):**

| Field | Definition |
|---|---|
| Token count | the cumulative value written by `context.ping`; compressed action logs keep the last ping for each job (including cumulative tokens). Cost is calculated as the difference between the job’s first_ping and last_ping |
| Duration | `job.completed_at - job.created_at`; unfinished jobs are estimated with `now() - created_at` |
| Failover attribution | tokens during failover are counted for the **new** provider (the old provider’s crash-time usage is already recorded in the old worker’s `context.ping`) |
| Cost after clean | compressed action-log lines (`[compressed] {date}: {n} actions, tokens: {total}`) preserve the token summary, so cost can be reconstructed from them |
| Cross-platform doctor fix commands | only output shell commands (sh/bash syntax), no PowerShell; macOS and Linux use the same command format; `export VAR=...` instead of `set VAR=...` |

---

**`kingdom restart` (`src/cli/restart.rs`):**

```
1. Send SIGTERM to the daemon
2. Wait up to 5 seconds
3. On timeout → SIGKILL
4. Restart the daemon (triggered by the watchdog or launched directly)
5. The daemon restores state from .kingdom/
6. Reconnect all surviving providers (15-second timeout)
7. Output: ✓ daemon restarted, session restored normally
```

Note: the tmux session and provider processes remain uninterrupted throughout.

---

**`kingdom swap` (full M7 supplement):**

```
kingdom swap {worker_id}              # show a provider selection list
kingdom swap {worker_id} {provider}   # specify directly
```

The provider selection list only shows installed providers within `available_providers`.

---

**`kingdom replay <job_id>` (`src/cli/replay.rs`):**

Read `job.intent`, create a new job with the same intent, and dispatch it directly:

```
kingdom replay job_001
  ↓
1. Read the intent for job_001
2. Call job.create(intent) (generates job_002)
3. If there is an available idle worker, ask: assign immediately? [Y/n]
4. Y → worker.assign(idle_worker, job_002)
5. Output: ✓ recreated job_002, intent: {intent[:60]}
```

Use case: when a job fails and you want to rerun it with the same intent without manual copy/paste.

---

**`kingdom job diff <job_id>` (`src/cli/job_diff.rs`):**

Show the git diff for a job from start to completion:

```
kingdom job diff job_001
  ↓
git diff {job.branch_start_commit}..HEAD -- {job.changed_files}
```

When `git strategy = None`, return the message: "This job ran in non-git mode and has no diff record."
Archived jobs (>90 days) are still viewable (the commit hash is recorded, and git preserves the diff history).

---

**`kingdom open <worker_id_or_job_id>` (`src/cli/open.rs`):**

Jump to the associated tmux pane:

```
kingdom open w1      # jump to w1's pane
kingdom open job_001 # jump to the worker pane that is executing job_001
```

Implementation: read `worker.pane_id` or find `pane_id` via `job.worker_id`, then call `tmux select-pane -t {pane_id}`.
If the pane no longer exists: show the message "pane closed (job has ended)."

---

**Notification Webhook (`src/notifications/webhook.rs`):**

config.toml configuration:

```toml
[notifications]
on_attention_required = "bell"    # existing field

[notifications.webhook]
url = "https://hooks.slack.com/..."  # optional
events = ["job.completed", "job.failed", "failover.triggered"]  # subscribed events
timeout_seconds = 5
```

payload format (HTTP POST, Content-Type: application/json):

```json
{
  "event": "job.completed",
  "job_id": "job_001",
  "worker": "Codex(w1)",
  "summary": "Implemented login validation, modified 3 files",
  "workspace": "/path/to/repo",
  "timestamp": "2026-04-06T10:30:00Z"
}
```

If the webhook call fails (timeout / 5xx), silently skip it, write a warning to the action log, and do not block the main flow.

### Acceptance Criteria

- [ ] `kingdom log` default view sorts by time descending and displays failover records correctly
- [ ] `kingdom log --detail job_001` shows the full timeline and checkpoint list
- [ ] `kingdom log --actions` parses and formats `action.jsonl` correctly
- [ ] `kingdom doctor` only checks system and config layers when the daemon is not running
- [ ] `kingdom doctor` displays `→ kingdom swap w1` when it detects a heartbeat-timeout worker
- [ ] `kingdom clean --dry-run` does not modify any files
- [ ] `kingdom clean` deletes expired checkpoints correctly after confirmation
- [ ] After `kingdom clean` compresses the action log, the original entries are removed and summary lines are written
- [ ] `kingdom cost` computes token usage from the action log and formats it correctly
- [ ] `kingdom cost` can still compute cost correctly from compressed token summaries after the action log is compressed
- [ ] Compressed action-log lines include a token-summary field
- [ ] `kingdom doctor` fix commands use `export VAR=...` syntax and do not contain platform-specific syntax
- [ ] After `kingdom restart`, the provider process PID remains unchanged
- [ ] `kingdom swap w1` without a provider argument shows the available provider list
- [ ] `kingdom replay job_001` creates a new job and carries over the original intent
- [ ] `kingdom job diff job_001` outputs the correct git diff (using a temporary git repo in tests)
- [ ] `kingdom job diff` outputs a friendly message when `git strategy = None`
- [ ] `kingdom open w1` calls `tmux select-pane` and switches to the pane (mock tmux verifies the call parameters)
- [ ] When a URL is configured, the `job.completed` event triggers an HTTP POST
- [ ] Webhook timeouts (5s) are silently skipped and do not block job completion
- [ ] Webhook failures write a warning entry to the action log

### Reference Docs

- `UX.md` §`kingdom log` Output Format
- `UX.md` §`kingdom doctor` Diagnostic Output
- `UX.md` §User Interaction Entry Points
- `ARCHITECTURE.md` §Storage Management
- `OPEN_QUESTIONS.md` Q23 (kingdom doctor)
- `OPEN_QUESTIONS.md` Q31 (kingdom log format)
- `OPEN_QUESTIONS.md` Q32 (kingdom restart)
- `OPEN_QUESTIONS.md` Q37 (kingdom clean)

---

## M10: End-to-End Integration

### Goal

Wire together all components from M1-M9, run full scenarios, and fix issues found during integration.
This milestone does **not** add new features; it only integrates, fixes, and handles edge cases.

### Test Scenarios

Each scenario needs automated integration tests (mock providers are allowed, but tmux operations must run for real).

---

**Scenario 1: Happy Path (single worker)**

```
1. kingdom up (choose claude as manager)
2. Manager calls job.create("implement a simple function")
3. Manager calls worker.create("codex")
4. Manager calls worker.assign(w1, job_001)
5. Worker(w1) calls job.progress("starting implementation")
6. Worker(w1) calls job.complete("implemented it, modified foo.rs")
7. Manager receives a notification and calls job.result(job_001)
8. kingdom log shows job_001 completed
9. kingdom down
```

Acceptance:
- [ ] Entire flow completes without error
- [ ] `state.json` shows job_001 status=Completed
- [ ] `action.jsonl` contains records of every step
- [ ] tmux status bar updates correctly at each step
- [ ] `kingdom log` shows the correct duration and worker information

---

**Scenario 2: Happy Path (parallel workers)**

```
1. kingdom up
2. Create job_001, job_002, job_003 (independent of each other)
3. Start 3 workers at the same time to execute them
4. The three jobs complete in different orders
5. Manager reviews the results one by one
6. kingdom down
```

Acceptance:
- [ ] 3 workers are laid out correctly in a 2x2 arrangement in the main window
- [ ] Each branch is independent (`kingdom/job_001`, job_002, job_003)
- [ ] Each worker’s `context.ping` is tracked independently

---

**Scenario 3: Job Dependency Chain**

```
1. job_001 (no dependencies)
2. job_002 (depends_on=[job_001])
3. job_003 (depends_on=[job_001, job_002])
4. job_001 completes → job_002 becomes Pending, manager receives a notification
5. job_002 completes → job_003 becomes Pending
6. job_001 is cancelled → manager receives a cascade warning
```

Acceptance:
- [ ] A job remains Waiting when its dependencies are not yet satisfied
- [ ] `ManagerNotification::JobUnblocked` is triggered correctly once dependencies are satisfied
- [ ] When `job.cancel` has cascading jobs, a warning log is written

---

**Scenario 4: Failover (context exhaustion)**

```
1. Worker executes a task, `context.ping` reports pct=0.91
2. Kingdom triggers failover
3. Worker performs a checkpoint first (urgency=Critical)
4. Popup shows: failure reason + handoff brief summary
5. User confirms the switch
6. A new provider starts in the same pane
7. HANDOFF separator appears in the pane
8. The new provider receives the handoff brief and continues work
9. The new provider completes the job
```

Acceptance:
- [ ] Checkpoint contents are complete in all five fields
- [ ] `possibly_incomplete_files` is identified correctly
- [ ] The new provider continues on the same branch
- [ ] HANDOFF separator has the correct format
- [ ] Circuit-breaker count increments

---

**Scenario 5: Session Restore**

```
1. kingdom up, start job_001 (running)
2. kingdom down --force (force exit without waiting for a checkpoint)
3. kingdom up
4. Show restore summary (job_001 paused)
5. User confirms continue
6. Manager restarts and receives the recovery context
7. Manager decides to resume job_001
```

Acceptance:
- [ ] `state.json` preserves job state after down
- [ ] `workspace.notes` persists across sessions
- [ ] Restore summary format is correct
- [ ] The manager receives the full recovery context (including notes + job state)

---

**Scenario 6: Kingdom daemon crash recovery**

```
1. kingdom up, with a running worker
2. Kill the Kingdom daemon directly (SIGKILL)
3. The watchdog restarts the daemon within 1 second
4. The provider reconnects automatically (exponential backoff)
5. After the daemon reconnects, session state is restored
6. The user notices nothing, and work continues
```

Acceptance:
- [ ] Watchdog completes restart within 2 seconds
- [ ] After provider reconnect, MCP tool calls work normally again
- [ ] Buffered tool calls during reconnect are replayed after recovery
- [ ] Status bar is restored after reconnect

---

**Scenario 7: Edge Cases**

```
7a. Non-git directory: kingdom up warns and degrades to strategy=none
7b. Missing API key: kingdom up warns but continues (provider still missing)
7c. Provider binary does not exist: worker.create fails and shows install guidance
7d. job.complete idempotent: calling it twice does not error
7e. worker.release on a Running worker: returns InvalidState
7f. file.read path traversal: returns an error
7g. More than 3 workers: the 4th worker opens in a new tmux window
7h. kingdom up finds an existing session: prompts to attach
```

Acceptance: each edge case has a corresponding test, and behavior matches the decisions in `OPEN_QUESTIONS.md`.

---

### Code Structure Checks

When M10 is complete, verify the following:

- [ ] The `src/` directory structure and module boundaries are reasonable (`types/`, `storage/`, `mcp/`, `process/`, `health/`, `failover/`, `tmux/`, `cli/`)
- [ ] No `unwrap()` / `expect()` in non-test code (all replaced with `?` or explicit error handling)
- [ ] No unused `#[allow(dead_code)]`
- [ ] `cargo clippy` reports no warnings
- [ ] `cargo test` passes (unit + integration tests)
- [ ] All write operations go through the Storage layer; there is no stray `std::fs::write` in business code

### Final Acceptance

- [ ] Scenarios 1-7 all pass
- [ ] `kingdom doctor` shows all green in a clean environment
- [ ] `kingdom log` correctly shows the failover record for scenario 4
- [ ] `kingdom clean --dry-run` shows the correct cleanup preview on test data
- [ ] Every decision for Q1-Q44 in `OPEN_QUESTIONS.md` has a corresponding implementation in code

### Reference Docs

- All design docs (final integration should follow the design docs)
- Full `OPEN_QUESTIONS.md` (verify item by item)
- `INTERFACES.md` §ID Format Specification (final verification of ID format consistency)

---

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| Eng Review | `/plan-eng-review` | Architecture & tests (required) | 1 | passed-with-changes | 8 issues found, 8 resolved, 1 deferred (distribution/CI) |
| CEO Review | `/plan-ceo-review` | Scope & strategy | 1 | passed-with-changes | SELECTIVE_EXPANSION; 8 proposals, 7 accepted, 1 deferred (headless) |
| Codex Review | `/codex review` | Independent 2nd opinion | 0 | — | — |
| Design Review | `/plan-design-review` | UI/UX gaps | 0 | — | — |

**ACCEPTED SCOPE (CEO review):** config hot-reload (M5), KINGDOM.md template (M5), provider stability history (M7), `kingdom replay` (M9), `kingdom job diff` (M9), `kingdom open` (M9), notification webhook (M9), status bar overflow handling (M8), manager stale pane UX (M8), prompt injection warning (M8)

**CRITICAL GAP (mitigated):** Prompt injection via `result_summary` → manager context. Mitigated in M8 with basic content filtering + action log warning.

**DEFERRED:** Headless mode (P3, TODOS.md), Distribution/CI pipeline (P2, TODOS.md)

**VERDICT:** ENG + CEO REVIEWS PASSED — ready to implement M1. Run `/plan-design-review` for a deep tmux/TUI UX review before M8 implementation.
