# Kingdom v2 接口契约

> 本文件是实现的"宪法"。所有 Rust 类型、MCP tool 签名、文件格式在此定义。
> Codex 实现时必须使用这里的类型，**不得修改签名**。修改签名需要 manager 审批并更新本文件。

---

## 核心数据类型

### Job

```rust
pub struct Job {
    pub id: String,                        // 格式："job_001"，session 内递增
    pub intent: String,                    // 用户原始描述
    pub status: JobStatus,
    pub worker_id: Option<String>,         // 当前分配的 worker
    pub depends_on: Vec<String>,           // job_id 列表
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub branch: Option<String>,            // git branch，strategy=branch 时有值
    pub branch_start_commit: Option<String>, // branch 创建时的 HEAD commit hash，用于计算 changed_files
    pub checkpoints: Vec<CheckpointMeta>,  // checkpoint 元数据列表（不含全文）
    pub result: Option<JobResult>,
    pub fail_count: u32,                   // 用于熔断计数
    pub last_fail_at: Option<DateTime<Utc>>,
}

pub enum JobStatus {
    Pending,      // 依赖已满足，等待 manager 分配
    Waiting,      // 有未完成依赖
    Running,      // worker 正在执行
    Completed,    // 已完成（job.complete 调用）
    Failed,       // 意外失败（crash / timeout / API error）
    Cancelled,    // 用户主动取消（job.cancel + job.cancelled()）
    Paused,       // 熔断或用户暂停，可恢复（job.keep_waiting / 手动 resume）
    Cancelling,   // 取消中（等待 worker graceful stop，30s 超时后变 Cancelled）
}

// 状态语义说明：
// - Failed 和 Cancelled 对依赖方的效果相同：依赖方保持 Waiting，manager 收到通知决定后续
// - Cancelled 不保存 result.json
// - Paused 保留所有 checkpoint，可被 job.update 重新激活

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
    pub git_commit: Option<String>,        // checkpoint 对应的 commit hash
}

// 完整内容存文件，不放内存
pub struct CheckpointContent {
    pub id: String,
    pub job_id: String,
    pub created_at: DateTime<Utc>,
    pub done: String,                      // 做了什么
    pub abandoned: String,                 // 放弃了什么及原因
    pub in_progress: String,               // 正在做什么
    pub remaining: String,                 // 还剩什么
    pub pitfalls: String,                  // 踩过哪些坑
    pub git_commit: Option<String>,
}
```

### Worker

```rust
pub struct Worker {
    pub id: String,                        // 格式："w1"，session 内递增，不回收
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
    pub permissions: Vec<Permission>,      // 当前授权的扩展权限
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
    pub workspace_hash: String,             // 路径 hash，用于 socket 命名
    pub manager_id: Option<String>,         // manager worker 的 id
    pub workers: HashMap<String, Worker>,
    pub jobs: HashMap<String, Job>,
    pub notes: Vec<WorkspaceNote>,
    pub worker_seq: u32,                    // 下一个 worker 序号（w{seq}）
    pub job_seq: u32,                       // 下一个 job 序号（job_{seq:03}）
    pub request_seq: u32,                   // 下一个 request 序号（req_{seq:03}）
    pub git_strategy: GitStrategy,
    pub available_providers: Vec<String>,   // 探测到已安装的 provider
    pub notification_mode: NotificationMode,
    pub pending_requests: HashMap<String, PendingRequest>,   // key = request_id
    pub pending_failovers: HashMap<String, PendingFailover>, // key = worker_id（每个 worker 最多一个待处理 failover）
    pub created_at: DateTime<Utc>,
}
```

### PendingRequest

```rust
pub struct PendingRequest {
    pub id: String,                         // 格式：req_001
    pub job_id: String,
    pub worker_id: String,                  // 发起请求的 worker
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
    WaitingConfirmation,               // 等待 manager/user 确认
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
    Poll,    // worker 轮询 job.request_status
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
    pub possibly_incomplete_files: Vec<String>,  // 崩溃时正在写入的文件
    pub changed_files: Vec<String>,
}
```

### 健康事件

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
        worker_id: String,      // 完成该 job 的 worker
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
        candidates: Vec<String>,  // 推荐的 provider 列表
    },
    WorkerIdle {
        worker_id: String,
    },
    WorkerReady {
        worker_id: String,
        provider: String,         // kingdom.hello 握手完成，Starting → Idle
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
// 追加写入 .kingdom/logs/action.jsonl，每行一个 JSON 对象
pub struct ActionLogEntry {
    pub timestamp: DateTime<Utc>,
    pub actor: String,     // worker_id | "kingdom" | "user"
    pub action: String,    // "job.complete" | "failover" | "worker.create" 等
    pub params: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}
```

---

## MCP Tool 签名

> 所有参数和返回值类型必须与此处一致。

### Manager 工具集

```
workspace.status() -> WorkspaceStatus
workspace.log(limit?: u32) -> Vec<ActionLogEntry>
workspace.note(content: String, scope?: String) -> String   // 返回 note_id
workspace.notes() -> Vec<WorkspaceNote>

worker.create(provider: String) -> String                   // 返回 worker_id
worker.assign(worker_id: String, job_id: String) -> ()
worker.release(worker_id: String) -> ()                     // 只能对 idle worker 调用
worker.swap(worker_id: String, new_provider: String) -> ()
worker.grant(worker_id: String, permission: String) -> ()
worker.revoke(worker_id: String, permission: String) -> ()

job.create(
    intent: String,
    worker_id?: String,       // 传则自动 assign
    depends_on?: Vec<String>
) -> String                                                 // 返回 job_id

job.status(job_id: String) -> JobStatusResponse
job.result(job_id: String) -> JobResultResponse             // 只在 completed 后有意义
job.cancel(job_id: String) -> ()
job.keep_waiting(job_id: String) -> ()
job.update(job_id: String, new_intent: String) -> ()
job.respond(request_id: String, answer: String) -> ()

failover.confirm(worker_id: String, new_provider: String) -> ()
failover.cancel(worker_id: String) -> ()
```

### Worker 工具集（默认最小权限）

```
job.progress(job_id: String, note: String) -> ()
job.complete(job_id: String, result_summary: String) -> ()  // 幂等
job.fail(job_id: String, reason: String) -> ()
job.cancelled() -> ()
job.checkpoint(job_id: String, summary: CheckpointSummary) -> ()
job.request(
    job_id: String,
    question: String,
    blocking: bool
) -> String                                                 // 返回 request_id
job.request_status(request_id: String) -> RequestStatus
job.status(job_id: String) -> JobStatusResponse             // 只能查自己的 job

file.read(path: String, lines?: String, symbol?: String) -> String
workspace.tree(path?: String) -> String
git.log(n?: u32) -> Vec<GitLogEntry>
git.diff(path?: String) -> String                          // 返回 unified diff

context.ping(usage_pct: f32, token_count: u64) -> ()
context.checkpoint_defer(
    job_id: String,
    reason: String,
    eta_seconds: u32
) -> ()
```

### Worker 扩展工具（需授权）

```
subtask.create(
    intent: String,
    depends_on?: Vec<String>
) -> String                                                 // 返回 job_id

worker.notify(
    target_worker_id: String,
    message: String
) -> ()

workspace.status() -> WorkspaceStatus                      // workspace.read 权限
job.list_all() -> Vec<JobSummary>                          // job.read_all 权限
```

---

## 返回值结构

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
    pub done: String,          // ≥20 字
    pub abandoned: String,     // ≥20 字
    pub in_progress: String,   // ≥20 字
    pub remaining: String,     // ≥20 字
    pub pitfalls: String,      // ≥20 字
}

pub struct GitLogEntry {
    pub hash: String,
    pub message: String,
    pub author: String,
    pub timestamp: DateTime<Utc>,
}
```

---

## 文件格式

### `.kingdom/state.json`——唯一 source of truth

**`state.json` 是唯一权威来源。** Session 的完整状态（含所有 Job、Worker、PendingRequest、PendingFailover）全部存在这里。

- 所有写操作先写 `state.json`，再做其他操作
- daemon 启动恢复时只读 `state.json`，不读 `jobs/*/meta.json`
- `jobs/*/meta.json` **不存在**（已废除，避免双写和不一致）

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

只存放过大、不适合放 state.json 的内容。不是 source of truth。

```
result.json        JobResult（completed 后写入，state.json 里 Job.result 同步写入）
handoff.md         最新交接简报（Markdown，仅供人类阅读）
checkpoints/
  {id}.json        CheckpointContent（每个 checkpoint 独立文件，完整内容）
```

> 注意：`state.json` 里 `Job.checkpoints` 只存 `Vec<CheckpointMeta>`（轻量元数据），
> 完整的 checkpoint 内容在 `jobs/{job_id}/checkpoints/{id}.json`。
> `Job.result` 在 `state.json` 里存完整 `JobResult`（size 可控），同时也写 `result.json` 供人类阅读。

### `.kingdom/logs/action.jsonl`

append-only，每行一个 ActionLogEntry JSON。

### `.kingdom/manager.json` / `.kingdom/worker.json`

MCP config 文件，由 `kingdom up` 生成：

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

Unix domain socket，Kingdom MCP server 监听地址。

---

## 配置格式（`.kingdom/config.toml`）

```toml
[workers]
main_window_max = 3            # 主 window 最大 worker 数

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

## ID 格式规范

| 实体 | 格式 | 示例 | 规则 |
|---|---|---|---|
| Job | `job_{seq:03}` | `job_001` | session 内递增，跨 session 不重置（从 state.json 读取） |
| Worker | `w{seq}` | `w1` | session 内递增不回收，`kingdom up` 时重置从 1 开始 |
| Session | `sess_{hash8}` | `sess_a3f9c2b1` | 随机 8 字符 hex |
| Checkpoint | `ckpt_{timestamp}` | `ckpt_20260405T143201` | ISO 时间戳 |
| Note | `note_{timestamp}` | `note_20260405T143201` | ISO 时间戳 |
| Request | `req_{seq:03}` | `req_001` | session 内递增 |
| Failover | `fov_{timestamp}` | `fov_20260405T143201` | ISO 时间戳 |

---

## MCP 协议定义

> 本节定义所有 JSON-RPC 消息的 method 名、payload 结构、幂等规则。

### 连接握手（kingdom.hello）

Provider 连接 Unix socket 后，**第一条消息必须是** `kingdom.hello`：

```json
// Provider → Kingdom（Request）
{
  "jsonrpc": "2.0",
  "id": "init",
  "method": "kingdom.hello",
  "params": {
    "role": "worker",          // "manager" | "worker"
    "worker_id": "w1",         // 已有 worker 重连时带原 worker_id（必须已由 worker.create 注册）
    "session_id": "sess_abc123"
  }
}

// Kingdom → Provider（Response）
{
  "jsonrpc": "2.0",
  "id": "init",
  "result": {
    "tools": ["job.progress", "job.complete", ...],   // 本 role 可用的工具列表
    "notification_mode": "push",                      // "push" | "poll"
    "queued_notifications": [                           // 断线期间积压的 notification，按时间顺序
      // 每项格式与 kingdom.event notification 相同：
      // { "method": "kingdom.event", "params": { "type": "...", "data": {...}, "text": "..." } }
      // 指令型通知（如 kingdom.cancel_job）不进入此队列（job 已 Cancelled，无需重放）
    ]
  }
}
```

**Kingdom 绑定连接到 worker_id 的规则：**
- `worker_id` 在 state.json 中存在 → 重连，更新 `worker.mcp_connected = true`
- `worker_id` 不存在 → 拒绝，返回错误（新 worker 必须由 `worker.create` 先注册）
- 同一 `worker_id` 的旧连接自动断开（新连接替代）

**session_id 不匹配时：** 返回错误，provider 退出（不允许跨 session 复用连接）。

---

### 取消协议（kingdom.cancel_job）

```
job.cancel(job_id) 被 manager 调用后：

1. Kingdom：job.status → Cancelling，写 state.json
2. Kingdom → Worker（Notification，无 id）：
   {
     "jsonrpc": "2.0",
     "method": "kingdom.cancel_job",
     "params": { "job_id": "job_001" }
   }
3. Worker 收到后：
   - 完成当前原子操作（不得中途截断文件写入）
   - 调用 job.cancelled()
   - 不再调用任何其他 job.* 工具
4. Kingdom 收到 job.cancelled()：
   - job.status → Cancelled
   - git stash（strategy != None 时，执行 git stash push -m "[kingdom cancelled] job_001"）
   - worker.status → Idle
   - 写 state.json + action log

超时规则（30 秒）：
- 30 秒内未收到 job.cancelled() → Kingdom 执行 SIGKILL
- job.status → Cancelled（强制）
- git stash（尽力，可能不完整）
- worker.status → Terminated

job.cancelled() 幂等：重复调用返回 Ok，状态不变。
```

**`kingdom.cancel_job` 的 worker 行为约束：**
- Worker 可以在收到通知后完成当前 tool call（如 file.read），但不得发起新的 tool call
- Worker 不得在收到取消后调用 job.complete 或 job.checkpoint

---

### 重连与 Replay 去重

**Provider 侧（断线重连）：**
- 连接断开时缓存所有未收到 Response 的 pending tool call（method + params + original id）
- 重连握手（kingdom.hello）成功后，按原顺序重发缓存的 tool call，使用**相同的 id**

**Kingdom 侧（去重）：**

```rust
// 内存结构，不持久化
pub struct RecentCalls {
    // key: (worker_id, jsonrpc_id)，其中 jsonrpc_id = JSON-RPC 消息的 "id" 字段
    // 不引入额外的 call_id 字段，直接复用 JSON-RPC id
    cache: HashMap<(String, String), serde_json::Value>,
    // TTL: 5 分钟，超过后条目自动清除
}
```

- 收到 tool call 时：先查 RecentCalls，key = `(worker_id, 消息 "id" 字段)`
  - 命中 → 直接返回缓存 result，**不重新执行**
  - 未命中 → 执行，写入缓存
- Kingdom 重启后 RecentCalls 清空（内存不持久化）；provider 重连时 kingdom.hello 的 `queued_notifications` 保证 notification 不丢，tool call replay 在 5 分钟窗口内

**TTL 超过的处理：**
- 超过 5 分钟的 replay → 当作新调用执行（provider 重连超过 5 分钟说明已超出正常重连场景）

---

### worker.assign 合法状态约束

```
合法调用条件：
  - worker.role == Worker（manager 不能被 assign）
  - worker.status == Idle
  - job.status == Pending

所有其他情况返回错误：

  worker.role == Manager
    → McpError::Unauthorized { reason: "cannot assign job to manager" }

  worker.status == Running（已有 job）
    → McpError::InvalidState { expected: "Idle", actual: "Running", detail: "worker already has job {job_id}" }

  worker.status == Starting
    → McpError::InvalidState { expected: "Idle", actual: "Starting", detail: "worker not yet ready" }

  worker.status == Terminated | Failed
    → McpError::InvalidState { expected: "Idle", actual: "{status}" }

  job.status != Pending
    → McpError::InvalidState { expected: "Pending", actual: "{status}" }
```

> Starting 状态不允许提前 assign。Manager 应等 `WorkerRunning` notification 后再 assign。

---

### job.cancel 最终状态与语义

```
状态转换（唯一规则）：

  用户调用 job.cancel(job_id)
  ├─ job 有 running worker → Cancelling
  │   ├─ worker 30s 内调用 job.cancelled() → Cancelled
  │   └─ 30s 超时 → Cancelled（强制 SIGKILL）
  └─ job 无 running worker（Pending/Waiting/Paused）→ 直接 Cancelled

Cancelled 状态的语义：
  - 不写 result.json
  - Job.result = None
  - 对依赖方：视为"未完成"，依赖方保持 Waiting
  - manager 收到 ManagerNotification::CancelCascade（若有依赖方）

Failed vs Cancelled vs Paused：
  Failed   = 意外失败（crash / heartbeat timeout / API error）
  Cancelled = 用户主动取消
  Paused   = 熔断或待恢复（job.keep_waiting 可重新激活）

  三者对依赖方的效果相同：依赖方保持 Waiting，manager 决定后续。
```

---

### changed_files 计算规则

```
strategy = "branch"：
  branch 创建时记录 Job.branch_start_commit = git rev-parse HEAD
  job.complete 时：git diff --name-only {branch_start_commit}..HEAD
  checkpoint commit 包含在此范围内（正确）

strategy = "commit"：
  job 开始时记录 Job.branch_start_commit = git rev-parse HEAD（在当前 branch）
  job.complete 时：git diff --name-only {branch_start_commit}..HEAD

strategy = "none"（含非 git 目录）：
  Kingdom 无法计算 changed_files
  worker 可在 result_summary 中自描述修改的文件（文本，不做验证）
  JobResult.changed_files = []（空列表，不接受 worker 自报）
```

---

### context.checkpoint_request 协议

Kingdom 主动向 worker 发送的 checkpoint 请求，是一条 **Kingdom → Worker 的 JSON-RPC notification**（无 id，worker 不需要 response）。

```json
// Kingdom → Worker（Notification，无 id）
{
  "jsonrpc": "2.0",
  "method": "kingdom.checkpoint_request",
  "params": {
    "job_id": "job_001",
    "urgency": "Normal" | "High" | "Critical"
  }
}
```

**`urgency` 类型**：直接复用 `CheckpointUrgency` 枚举：

```rust
pub enum CheckpointUrgency {
    Normal,    // context ≥ 50%，建议 checkpoint，可延迟
    High,      // context ≥ 70%，强烈建议，延迟窗口短
    Critical,  // context ≥ 85%，必须立刻 checkpoint，不允许延迟
}
```

**Worker 收到后的合法响应路径：**

| urgency | 允许的响应 |
|---|---|
| Normal | 调用 `job.checkpoint()`，或调用 `context.checkpoint_defer(eta_seconds ≤ 60)` |
| High | 调用 `job.checkpoint()`，或调用 `context.checkpoint_defer(eta_seconds ≤ 15)` |
| Critical | 只能调用 `job.checkpoint()`；调用 `context.checkpoint_defer()` 时 Kingdom 返回 `ValidationFailed` |

Worker 也可以忽略通知（例如正处于原子操作中），Kingdom 通过超时检测处理。

**Kingdom 侧的超时与降级：**

```
Kingdom 发出 kingdom.checkpoint_request(urgency)
  ↓
等待窗口（Normal: 60s / High: 15s / Critical: 立刻，无等待）
  ↓
窗口内 worker 调用 job.checkpoint()       → 成功，重置 context threshold 计数
窗口内 worker 调用 context.checkpoint_defer → 记录延迟，更新下次触发时间
窗口超时 worker 无响应                     → 降级：Kingdom 用 git diff 生成 fallback checkpoint
                                             （五项留空，标注 "[自动生成，无摘要]"）
```

**幂等规则：**
- 同一 worker 处于 checkpoint 等待中时，Kingdom 不重复发送 `kingdom.checkpoint_request`
- Worker 完成 checkpoint 后才重置等待状态，允许下一次阈值触发

**与 `kingdom.cancel_job` 的关系：**
- Worker 收到 `kingdom.checkpoint_request` 后，若同时收到 `kingdom.cancel_job`，优先处理取消（完成当前原子操作后调用 `job.cancelled()`，不需要先 checkpoint）
