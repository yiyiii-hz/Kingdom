# Kingdom v2 Build Plan

> English version: [BUILD_PLAN.en.md](./BUILD_PLAN.en.md)

> 本文件定义实现顺序和每个 milestone 的验收条件。
> Codex 每次只做一个 milestone，完成后由 manager 验收，通过后再开始下一个。

---

## 技术选型

### 语言与工具链

```
语言：Rust（edition 2021）
最低版本：1.75.0（支持 async fn in trait）
```

### 项目结构

单一 workspace，两个 binary crate：

```
Kingdom/
  Cargo.toml          workspace 定义
  Cargo.lock
  crates/
    kingdom/          主程序（daemon + CLI 合一）
      Cargo.toml
      src/
        main.rs       入口，解析子命令
        cli/          CLI 命令（up/down/log/doctor 等）
        mcp/          MCP server + 工具集
        storage/      .kingdom/ 读写
        process/      provider 启动 + PID 追踪
        health/       健康监控
        failover/     failover 状态机
        tmux/         tmux 操作
        types.rs      所有共享数据类型
        config.rs     config.toml 解析
    kingdom-watchdog/ watchdog 进程（独立 binary，极轻量）
      Cargo.toml
      src/
        main.rs
```

`kingdom` binary 集 daemon + CLI 于一体：
- `kingdom up` → 启动 watchdog，watchdog 启动 daemon，daemon 在后台运行
- `kingdom log` / `kingdom doctor` 等 → 连接已运行的 daemon（via Unix socket）读取状态

### 关键依赖

```toml
[dependencies]
# 异步运行时
tokio = { version = "1", features = ["full"] }

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"

# CLI
clap = { version = "4", features = ["derive"] }

# 错误处理
anyhow = "1"
thiserror = "1"

# 时间
chrono = { version = "0.4", features = ["serde"] }

# 进程 / 系统调用
nix = { version = "0.27", features = ["process", "signal"] }

# 日志
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# 随机（session ID 生成）
rand = "0.8"

# Async trait dispatch（Tool trait 需要 Box<dyn Tool> + async fn，RPITIT 不支持 object safety）
async-trait = "0.1"

[dev-dependencies]
mockall = "0.12"
tempfile = "3"
tokio-test = "0.4"
```

### MCP 协议实现方式

**不使用外部 MCP SDK，自行实现 JSON-RPC 2.0 over Unix domain socket。**

理由：
- 现有 Rust MCP SDK（rmcp）的 Unix socket transport 支持不稳定
- Kingdom 同时作为 server（对 provider）和 client（对 provider 发 notification），自实现控制更完整
- JSON-RPC 协议本身极简单，自实现约 200 行

消息格式（标准 JSON-RPC 2.0）：

```json
// Request（Kingdom 接收 or 发出）
{ "jsonrpc": "2.0", "id": 1, "method": "job.complete", "params": { ... } }

// Response
{ "jsonrpc": "2.0", "id": 1, "result": { ... } }

// Error
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32600, "message": "..." } }

// Notification（无 id，Kingdom → Provider，单向推送）
// 事件通知用 kingdom.event（信息型，manager 和 worker 都可能收到）
{ "jsonrpc": "2.0", "method": "kingdom.event", "params": { "type": "job_completed", "data": { ... } } }

// 指令型通知用具体方法名（需要 worker 执行特定行为）
{ "jsonrpc": "2.0", "method": "kingdom.cancel_job", "params": { "job_id": "job_001" } }
```

实现位置：`crates/kingdom/src/mcp/jsonrpc.rs`

### Watchdog 设计

`kingdom-watchdog` 是一个极轻量的独立进程（约 100 行）：

```rust
// 职责：启动并监控 kingdom daemon，崩溃后立刻重启
// 不做任何状态管理，状态全在 .kingdom/ 里

fn main() {
    let daemon_path = std::env::args().nth(1).unwrap(); // kingdom daemon binary 路径
    let workspace = std::env::args().nth(2).unwrap();   // workspace 路径

    loop {
        let mut child = Command::new(&daemon_path)
            .arg("--daemon")
            .arg(&workspace)
            .spawn()
            .expect("failed to start daemon");

        let status = child.wait().unwrap();

        if status.success() {
            break; // 正常退出（kingdom down），watchdog 也退出
        }

        // 非正常退出 → 立刻重启
        eprintln!("[watchdog] daemon exited ({:?}), restarting...", status);
        std::thread::sleep(Duration::from_millis(500));
    }
}
```

Watchdog 由 `kingdom up` 启动，PID 写入 `.kingdom/watchdog.pid`。

## 总览

| Milestone | 名称 | 核心交付 |
|---|---|---|
| M1 | 数据类型 + 存储层 | Rust 类型定义、state.json 读写 |
| M2 | MCP Server 骨架 | Unix socket、工具分发、连接管理 |
| M3 | Manager 工具集 | 所有 manager MCP 工具实现 |
| M4 | Worker 工具集 | 所有 worker MCP 工具实现 |
| M5 | Process Manager | Provider 启动、kingdom up/down |
| M6 | 健康监控 | 心跳、PID 监控、progress 超时 |
| M7 | Failover 状态机 | 检测→切换全流程 |
| M8 | tmux 集成 | Status bar、popup、pane 管理 |
| M9 | CLI 命令 | log / doctor / clean / cost / restart |
| M10 | 端到端集成 | Happy path + failover 场景测试 |

## 依赖顺序

```
M1 → M2 → M3 → M4 → M5 → M6 → M7
                              ↓
                    M8 → M9 → M10
```

M3 和 M4 可并行（在 M2 之后）。

---

## M1：数据类型 + 存储层

### 目标

实现 INTERFACES.md 中定义的所有 Rust 类型，以及对 `.kingdom/` 目录的读写操作。
这是其他所有 milestone 的基础，不包含任何网络、进程、tmux 逻辑。

### 实现范围

**类型定义（`src/types.rs`）：**
- `Job`, `JobStatus`, `JobResult`, `CheckpointMeta`, `CheckpointContent`
- `Worker`, `WorkerRole`, `WorkerStatus`, `Permission`
- `Session`, `WorkspaceNote`, `NoteScope`, `GitStrategy`
- `FailoverRequest`, `FailoverReason`, `HandoffBrief`
- `PendingRequest`, `PendingFailover`, `PendingFailoverStatus`（INTERFACES.md §MCP 协议定义）
- `HealthEvent`, `CheckpointUrgency`
- `ManagerNotification`
- `ActionLogEntry`
- `RecentCalls`（replay 去重缓存，HashMap<(worker_id, jsonrpc_id), Instant>）
- 所有返回值结构（`WorkspaceStatus`, `JobSummary`, `WorkerSummary` 等）
- 所有 ID 类型（`String` newtype 或 alias）

**`JobStatus` 枚举必须包含 `Cancelled` 变体**（用户主动取消，与 `Failed` 语义不同，可在 history 中审计）：
```rust
pub enum JobStatus {
    Pending, Waiting, Running, Completed,
    Failed,     // unexpected（崩溃/超时/API 错误）
    Cancelled,  // 用户主动取消
    Paused,     // circuit breaker 或用户暂停（可恢复）
    Cancelling, // graceful stop 进行中
}
```

**`ManagerNotification` 枚举必须包含 `WorkerReady` 变体**（worker 完成 `kingdom.hello` 握手后 status 从 Starting → Idle 时触发）：
```rust
pub enum ManagerNotification {
    JobCompleted { job_id: String, worker_id: String, summary: String, changed_files: Vec<String> },
    JobFailed { job_id: String, worker_id: String, reason: String },
    WorkerRequest { job_id: String, request_id: String, question: String, blocking: bool },
    JobUnblocked { job_id: String },
    FailoverReady { worker_id: String, reason: FailoverReason, candidates: Vec<String> },
    WorkerIdle { worker_id: String },
    WorkerReady { worker_id: String, provider: String },  // Starting → Idle（握手完成）
    SubtaskCreated { parent_job_id: String, subtask_job_id: String, intent: String },
}
```

**`Session` 必须包含以下字段**（state.json 是唯一状态源）：
```rust
pub struct Session {
    // ... 其他字段 ...
    pub request_seq: u32,
    pub pending_requests: HashMap<String, PendingRequest>,
    pub pending_failovers: HashMap<String, PendingFailover>,
}
```

所有类型必须实现：
- `serde::Serialize` + `serde::Deserialize`
- `Debug`, `Clone`
- `PartialEq` （用于测试断言）

**存储层（`src/storage.rs`）：**

```rust
pub struct Storage {
    pub root: PathBuf,  // .kingdom/ 目录路径
}

impl Storage {
    pub fn init(workspace: &Path) -> Result<Self>;    // 创建 .kingdom/ 目录结构 + .gitignore
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

**目录结构（`Storage::init` 创建）：**

```
.kingdom/
  .gitignore         内容：*
  state.json         唯一状态源（jobs/*/meta.json 不存在）
  logs/
    action.jsonl     初始为空
  jobs/              每个 job 的 checkpoint + handoff + result 文件
```

**状态设计约束：** `state.json` 是全局唯一状态源。`jobs/*/meta.json` **不存在**，所有 job 元数据存在 `state.json` 的 `jobs` 字段。`jobs/{id}/` 目录只存放 checkpoint/handoff/result 等大文件。`Storage::load_job` 从 `state.json` 读取，不读独立文件。

### 不实现

- MCP server / client
- 进程启动
- tmux 操作
- CLI 命令

### 验收条件

- [ ] `cargo test` 全部通过
- [ ] 所有类型可以序列化再反序列化，结果一致（round-trip 测试）
- [ ] `Storage::init` 在新目录和已有目录均正常运行
- [ ] `save_session` + `load_session` round-trip 正确
- [ ] `save_job` + `load_job` round-trip 正确
- [ ] `save_checkpoint` + `load_checkpoint` round-trip 正确
- [ ] `append_action_log` 追加后 `read_action_log` 可读取所有条目
- [ ] 测试覆盖所有状态枚举的序列化，包括 `JobStatus::Cancelled`
- [ ] `PendingRequest` + `PendingFailover` round-trip 序列化正确
- [ ] `Session` 含 `pending_requests` / `pending_failovers` / `request_seq` 字段后仍可 round-trip
- [ ] `Storage::load_job` 从 `state.json` 读取（不依赖 `jobs/*/meta.json`）

### 参考文档

- `INTERFACES.md` §核心数据类型
- `INTERFACES.md` §文件格式
- `INTERFACES.md` §ID 格式规范
- `INTERFACES.md` §MCP 协议定义（`PendingRequest`, `PendingFailover`）
- `ARCHITECTURE.md` §数据一致性

---

## M2：MCP Server 骨架

### 目标

实现 Kingdom MCP server 的连接管理和工具分发框架。
工具只需要注册和骨架（返回假数据），**不实现业务逻辑**。

### 实现范围

**MCP Server（`src/mcp/server.rs`）：**

```rust
pub struct McpServer {
    socket_path: PathBuf,
    // 连接注册表：connection_id → ConnectedClient
}

pub struct ConnectedClient {
    pub connection_id: String,
    pub worker_id: Option<String>,
    pub role: WorkerRole,
    pub session_id: String,
}

impl McpServer {
    pub fn new(workspace_hash: &str) -> Self;
    pub async fn start(&self) -> Result<()>;   // 开始监听 Unix socket
    pub async fn stop(&self) -> Result<()>;
}
```

**连接握手（`kingdom.hello` 协议）：**

Client 连接后必须先发送 `kingdom.hello` 请求，Kingdom 返回 response 后连接才进入就绪状态：

```jsonc
// Client → Kingdom（与 INTERFACES.md §MCP 协议定义完全一致）
{
  "jsonrpc": "2.0", "id": "init", "method": "kingdom.hello",
  "params": {
    "role": "manager" | "worker",
    "session_id": "sess_abc123",
    "worker_id": "w1"   // worker 必传；manager 省略。重连和首次连接格式相同，Kingdom 通过 state.json 判断
  }
}

// Kingdom → Client（success）
{
  "jsonrpc": "2.0", "id": "init",
  "result": {
    "tools": ["job.progress", ...],   // 该 role 可用的工具列表
    "notification_mode": "push" | "poll",
    "queued_notifications": [         // 断线期间积压的 kingdom.event，按时间顺序，可为空数组
      { "method": "kingdom.event", "params": { "type": "...", "data": {}, "text": "..." } }
    ]
  }
}
```

Kingdom 在 `kingdom.hello` 处理中：
1. 验证 `session_id` 与当前 session 匹配，不匹配返回错误，断开连接
2. Worker 角色验证 `worker_id` 在 state.json 中存在 → 重连（更新 `mcp_connected = true`）
3. Worker 角色验证 `worker_id` 在 state.json 中**不存在** → 拒绝，返回错误（新 worker 必须先由 manager 调用 `worker.create` 注册，不允许在握手时隐式创建）
4. 绑定 connection → worker_id，同一 `worker_id` 的旧连接自动断开
5. 返回 `notification_mode`：M2/M3/M4 统一返回 `"poll"`，M8 实现推送后改为 `"push"`
6. 返回 `queued_notifications`：断线期间积压的 `kingdom.event` 列表（M8 实现，M2 返回空数组）

**工具分发（`src/mcp/dispatcher.rs`）：**

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

**错误处理（`src/mcp/error.rs`）：**

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

**重放去重（`src/mcp/replay.rs`）：**

```rust
pub struct RecentCalls {
    // key: (worker_id, jsonrpc_id)，即 JSON-RPC 消息的 "id" 字段，TTL 5 分钟
    // 不引入额外的 call_id 字段；provider 保证同一连接内 id 唯一
    calls: HashMap<(String, String), (Instant, serde_json::Value)>,
}

impl RecentCalls {
    pub fn check(&self, worker_id: &str, jsonrpc_id: &str) -> Option<&serde_json::Value>;
    pub fn insert(&mut self, worker_id: &str, jsonrpc_id: &str, result: serde_json::Value);
    pub fn evict_expired(&mut self);  // 每次 insert 时顺带清理过期条目
}
```

Provider 断线重连时**用相同的 JSON-RPC id 重发**未收到 response 的 tool call，Kingdom 命中 `RecentCalls` 则直接返回缓存结果，不重复执行。无需任何额外字段。

**骨架工具注册：**
- 注册 INTERFACES.md 中所有 manager 工具和 worker 工具
- 每个工具返回 `serde_json::Value::Null` 或空结构
- 权限检查必须实现：manager 工具被 worker 调用时返回 `McpError::Unauthorized`

**Socket 路径：**
```
/tmp/kingdom/{workspace_hash}.sock        # MCP socket（provider 专用）
/tmp/kingdom/{workspace_hash}-cli.sock    # CLI socket（kingdom log / doctor / clean 专用）
```

**CLI Socket（`src/mcp/cli_server.rs`）：**

与 MCP socket 完全独立。CLI 命令（`kingdom log`, `kingdom doctor`, `kingdom restart` 等）通过此 socket 向 daemon 发送 JSON 请求并接收 JSON 响应。不使用 JSON-RPC，简单 request/response：

```json
// CLI → Daemon
{ "cmd": "status" }
{ "cmd": "log", "limit": 50 }
{ "cmd": "shutdown" }
{ "cmd": "ready" }          // kingdom up 用于轮询 daemon 是否就绪

// Daemon → CLI
{ "ok": true, "data": { ... } }
{ "ok": false, "error": "daemon not initialized" }
```

Daemon 启动时同时监听两个 socket。`kingdom up` 通过 CLI socket 的 `ready` 命令轮询（100ms 间隔，15s 超时）确认 daemon 就绪，再启动 manager provider。

**出站推送（`src/mcp/push.rs`）：**

M2 实现 worker-directed 推送基础设施（`kingdom.cancel_job`、`kingdom.checkpoint_request` 在 M3/M6 需要）。Manager event 推送（`kingdom.event`）在 M8 完成。

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

### 不实现

- 工具的实际业务逻辑（M3/M4 实现）
- Kingdom → Manager 的 `kingdom.event` 推送（M8，仅 manager notification，非 worker 指令）
- 心跳追踪
- 进程管理

### 验收条件

- [ ] `cargo test` 全部通过
- [ ] 可以用测试 MCP client 连接 socket，发送 `kingdom.hello` 完成握手
- [ ] 握手响应包含 `tools`、`notification_mode`、`queued_notifications`（空数组）三个字段
- [ ] 握手 `session_id` 不匹配时返回错误，连接断开
- [ ] 握手 `worker_id` 在 state.json 中不存在时返回错误（不允许隐式创建）
- [ ] 握手后收到正确的工具列表（manager 和 worker 各自不同）
- [ ] Worker 调用 manager 专属工具时返回 `Unauthorized` 错误
- [ ] 所有 INTERFACES.md 中的工具名均已注册（即使是骨架）
- [ ] 重放去重：相同 `(worker_id, jsonrpc_id)` 第二次调用返回缓存结果，不触发业务逻辑
- [ ] `RecentCalls` TTL 过期后条目被清理
- [ ] Server 异常退出后 socket 文件被清理
- [ ] 同一 session_id 可以有多个 client 同时连接

### 参考文档

- `INTERFACES.md` §MCP Tool 签名
- `INTERFACES.md` §MCP 协议定义（握手、重连、replay 去重）
- `ARCHITECTURE.md` §MCP Socket 管理
- `ARCHITECTURE.md` §两个独立通道
- `MCP_CONTRACT.md` §设计原则

---

## M3：Manager 工具集

### 目标

实现所有 manager MCP 工具的业务逻辑。工具调用读写 `Storage`，更新 `Session` 状态，写入 action log。
本 milestone 没有真实 worker 进程，所有 worker 状态通过测试直接写入 `state.json`。

### 实现范围

**实现以下工具（`src/mcp/tools/manager/`）：**

`workspace.status`
- 从 Storage 读取当前 Session
- 返回 `WorkspaceStatus`

`workspace.log`
- 从 `action.jsonl` 读取最近 N 条（默认 50）
- 支持 `limit` 参数

`workspace.note`
- 创建 `WorkspaceNote`，写入 Session
- scope 解析：`"global"` / 路径字符串 / `"job:{job_id}"`
- 持久化到 `state.json`

`workspace.notes`
- 返回所有 notes，按 scope 优先级排序（job > directory > global）

`job.create`
- 生成 job_id（`job_{seq:03}`，seq 从 state.json 读取并递增）
- 如果 `git.strategy == Branch`：创建 `kingdom/{job_id}` branch，记录 `branch_start_commit = git rev-parse HEAD`
- 如果 `git.strategy == Commit`：不切 branch，同样记录 `branch_start_commit = git rev-parse HEAD`（用于 changed_files 计算）
- 如果 `git.strategy == None`：`branch_start_commit = None`
- 检查 `depends_on` 中的 job_id 是否存在，不存在则报错
- 依赖全部 completed → status=Pending，否则 status=Waiting
- 如果传了 `worker_id`：自动调用 assign 逻辑
- 写入 `state.json` + action log

`job.status`
- 返回 `JobStatusResponse`

`job.result`
- job 不是 completed 时返回 `InvalidState` 错误
- 从 `.kingdom/jobs/{job_id}/result.json` 读取

`job.cancel`
- 检查是否有依赖该 job 的其他 job（waiting 状态）
- 有则在 action log 记录 cascade 警告（不自动取消，由 manager 决定）
- 有 worker 在跑：job 状态改为 Cancelling，Kingdom 发送 `kingdom.cancel_job` notification 给 worker（Phase 1 graceful stop）
- 无 worker（Pending/Waiting/Paused）：job 状态直接改为 **Cancelled**（不是 Failed）
- 写 action log

`kingdom.cancel_job` notification 格式（Kingdom → Worker）：
```json
{
  "jsonrpc": "2.0",
  "method": "kingdom.cancel_job",
  "params": { "job_id": "job_001", "reason": "manager_cancelled" }
}
```

Worker 收到后调用 `job.cancelled()` 确认，Kingdom 将 job 改为 `Cancelled`（30s 超时后强制 kill，状态也改为 `Cancelled`）。

`job.keep_waiting`
- job 状态改回 Waiting（用于依赖失败后 manager 选择继续等待）

`job.update`
- 更新 intent，如果 job 是 Waiting/Paused/Failed 状态则改为 Pending

`job.respond`
- 找到对应的 pending request，写入 answer
- 标记 request 为 answered
- 写 action log

`worker.assign`
- 验证 worker 存在且是 **Idle** 状态（一个 worker 同时只能跑一个 job，Running 状态返回 `InvalidState { code: "WORKER_BUSY" }`）
- 验证 job 存在且是 **Pending** 状态（Waiting/Running/Completed/Failed/Cancelled 均返回对应错误码）
- 更新 job.worker_id，job.status → Running
- 更新 worker.job_id，worker.status → Running
- 写 action log

合法状态约束（完整表，见 INTERFACES.md §MCP 协议定义）：
| worker 状态 | 结果 |
|---|---|
| Idle | ✓ 允许 |
| Running | ✗ `WORKER_BUSY` |
| Terminated | ✗ `WORKER_NOT_FOUND` |
| 不存在 | ✗ `WORKER_NOT_FOUND` |

`worker.release`
- 验证 worker 是 Idle 状态，否则返回 `InvalidState`
- 更新 worker.status → Terminated（实际 kill 进程在 M5 实现）
- 写 action log

`worker.grant` / `worker.revoke`
- 更新 worker.permissions 列表
- 通知 dispatcher 更新该 worker 的可用工具集

`failover.confirm` / `failover.cancel`
- 存储待处理 failover 的用户决策
- 实际切换逻辑在 M7 实现

**action log 记录规范：**
- 每个工具调用成功后写一条 ActionLogEntry
- actor = manager 的 worker_id
- action = 工具名（如 `"job.create"`）
- params = 调用参数（脱敏：不记录 answer 内容，只记录 request_id）

### 不实现

- 实际启动/终止进程（worker.create / worker.release 只写状态）
- worker.create（进程启动在 M5）
- worker.swap（failover 在 M7）
- Kingdom → Manager notification 推送
- tmux 操作

### 验收条件

- [ ] `cargo test` 全部通过
- [ ] `job.create` → `job.status` 返回正确状态
- [ ] `job.create` 有依赖时，`depends_on` job 不存在返回错误
- [ ] `job.create` 依赖全部 completed 时 status=Pending，否则 Waiting
- [ ] `job.create` 传 `worker_id` 时自动 assign
- [ ] `workspace.note` 持久化后 `workspace.notes` 可读取
- [ ] `worker.assign` 对 Running worker 返回 `InvalidState { code: "WORKER_BUSY" }`
- [ ] `worker.assign` 对 Pending job 以外的 job 返回对应错误
- [ ] `worker.release` 对 Running worker 返回 InvalidState
- [ ] `worker.grant` 后 dispatcher 下发的工具列表包含新权限工具
- [ ] 所有工具调用均写入 action log，`workspace.log` 可读取
- [ ] `job.cancel` 有 worker 时 job 变为 Cancelling 并发出 `kingdom.cancel_job` notification
- [ ] `job.cancel` 无 worker（Pending/Waiting）时 job 直接变为 Cancelled（不是 Failed）
- [ ] `job.cancel` 有级联 job 时写入 cascade 警告日志
- [ ] `worker.create` 注册 worker 后，收到 `kingdom.hello` 握手时触发 `ManagerNotification::WorkerReady`

### 参考文档

- `INTERFACES.md` §MCP Tool 签名
- `INTERFACES.md` §返回值结构
- `MCP_CONTRACT.md` §Manager 工具集
- `CORE_MODEL.md` §Job 依赖
- `OPEN_QUESTIONS.md` Q30（job 取消级联）
- `OPEN_QUESTIONS.md` Q44（job.create + assign 合并）

---

## M4：Worker 工具集

### 目标

实现所有 worker MCP 工具的业务逻辑。包括任务汇报、checkpoint、context 追踪、文件读取。
本 milestone 依赖 M3 完成（job 状态由 M3 的工具维护）。

### 实现范围

**实现以下工具（`src/mcp/tools/worker/`）：**

`job.progress`
- 更新 `worker.last_progress` 时间戳
- 写 action log（含 note 内容）
- 向 manager 发送 notification（本 milestone 只写队列，推送在 M8）

`job.complete`
- **幂等**：已经 completed 的 job 再次调用直接返回 Ok，不报错
- 验证 result_summary 非空且 ≥20 字，否则返回 ValidationFailed
- 更新 job.status → Completed，写入 result.json
- 附加 changed_files（来源取决于 git strategy，见 INTERFACES.md §changed_files 计算规则）：
  - `Branch`：`git diff --name-only {job.branch_start_commit}..HEAD`（`branch_start_commit` = 创建 branch 时的 commit）
  - `Commit`：`git diff --name-only {job.branch_start_commit}..HEAD`（`branch_start_commit` = job 开始时 `git rev-parse HEAD`，记录方式与 Branch 相同，只是不切 branch）
  - `None`：空列表 `[]`（Kingdom 无法计算，worker 可在 result_summary 文本中自描述，但 `changed_files` 字段始终为空）
- worker.status → Idle
- 写 action log
- 加入 manager notification 队列：`ManagerNotification::JobCompleted`
- 检查是否有 waiting job 的依赖被满足，若有则将其改为 Pending 并加入 notification 队列：`JobUnblocked`

`job.fail`
- 更新 job.status → Failed
- worker.status → Idle
- 写 action log
- 加入 manager notification 队列：`ManagerNotification::JobFailed`

`job.cancelled`
- 确认 graceful stop 完成（响应 job.cancel 的两阶段 shutdown）
- job.status → **Cancelled**（不是 Failed；Cancelled 表示用户主动取消，语义独立）
- worker.status → Idle
- 写 action log

`job.checkpoint`
- 验证 CheckpointSummary 五项均非空且每项 ≥20 字，否则返回 ValidationFailed
- 保存 CheckpointContent 到 `.kingdom/jobs/{job_id}/checkpoints/{id}.json`
- 如果 `git.strategy != None`：执行 `git add -A && git commit -m "[kingdom checkpoint] {job_id}: {done 前 50 字}"`
- 更新 job.checkpoints 列表
- 写 action log

`job.request`
- 生成 request_id（`req_{seq:03}`）
- 存入 Session.pending_requests
- 加入 manager notification 队列：`ManagerNotification::WorkerRequest`
- 返回 request_id

`job.request_status`
- 查找 pending_requests 中的 request
- 返回 `RequestStatus { answered: bool, answer: Option<String> }`

`job.status`（worker 版）
- 只允许查询自己当前分配的 job，查其他 job 返回 Unauthorized

`context.ping`
- 更新 worker.context_usage_pct 和 worker.token_count
- 更新 worker.last_heartbeat
- 根据 usage_pct 触发 HealthEvent：
  - ≥0.50 → `ContextThreshold { urgency: Normal }`
  - ≥0.70 → `ContextThreshold { urgency: High }`
  - ≥0.85 → `ContextThreshold { urgency: Critical }`
  - ≥0.90 → 加入 failover 候选队列（M7 处理）
- 写入 HealthEvent 队列（M6 消费）

`context.checkpoint_defer`
- 响应 Kingdom 发出的 `kingdom.checkpoint_request` notification（worker 主动调用此工具告知 Kingdom 暂时无法 checkpoint）
- urgency=Critical 时返回 ValidationFailed（不允许延迟，见 INTERFACES.md §context.checkpoint_request 协议）
- urgency=Normal/High 时记录延迟请求，更新下次触发时间（Normal: +60s，High: +15s）
- 写 action log

`file.read`
- 读取 workspace_path 下的文件
- `lines` 参数格式：`"100-300"`，解析为行范围
- `symbol` 参数：M4 不做 AST 解析；有 `symbol` 时降级为全文读取，行为规则：
  1. 返回内容头部追加：`# [symbol lookup not supported in M4, falling back to full file read]`
  2. 写一条 action log：`{ action: "file.read.symbol_fallback", params: { path, symbol } }`
  3. 不报错（M10 可扩展为真正 AST 解析）
- 默认：前 200 行
- 路径必须在 workspace_path 内（防止路径穿越）

`workspace.tree`
- 执行 `find {path} -maxdepth 3 -not -path '*/\.*'`
- 返回目录树字符串
- 默认 path = workspace_path

`git.log`
- 执行 `git -C {workspace_path} log --oneline -{n}`
- 解析为 `Vec<GitLogEntry>`

`git.diff`
- 执行 `git -C {workspace_path} diff {path}`
- 返回 unified diff 字符串

**扩展工具（需授权）：**

`subtask.create`
- 需要 Permission::SubtaskCreate，否则 Unauthorized
- 调用 job.create 逻辑，强制追加 `depends_on=[caller 的 job_id]`
- creator 记录为 caller 的 worker_id
- 加入 notification 队列：`ManagerNotification::SubtaskCreated`

`worker.notify`
- 需要 Permission::WorkerNotify，否则 Unauthorized
- 将消息加入目标 worker 的 notification 队列（M8 推送）

### 不实现

- HealthEvent 的实际处理（M6）
- ManagerNotification 的实际推送（M8）
- `symbol` 参数的 AST 解析（可在 M10 扩展）

### 验收条件

- [ ] `cargo test` 全部通过
- [ ] `job.complete` 幂等：连续调用两次返回 Ok，job 状态不变
- [ ] `job.complete` result_summary < 20 字时返回 ValidationFailed
- [ ] `job.complete` 在 Branch strategy 下 changed_files 通过 `branch_start_commit` 计算（测试用临时 git repo）
- [ ] `job.complete` 在 Commit strategy 下 changed_files 同样通过 `branch_start_commit` 计算（不切 branch，仅记录起始 commit）
- [ ] `job.complete` 在 None strategy 下 changed_files 为空列表
- [ ] `job.complete` 后依赖该 job 的 waiting job 变为 Pending
- [ ] `job.checkpoint` 五项有一项 < 20 字时返回 ValidationFailed
- [ ] `job.checkpoint` 在 git 模式下产生 commit（测试用临时 git repo）
- [ ] `context.ping` usage_pct=0.72 时触发 HealthEvent（urgency=High）
- [ ] `context.checkpoint_defer` urgency=Critical 时返回 ValidationFailed
- [ ] `job.request` blocking=true 时 HTTP/JSON-RPC 连接保持打开（长轮询），直到 `job.respond` 被调用后才返回 response
- [ ] `job.request` blocking=false 时立即返回 request_id，不等待 answer
- [ ] `file.read` 路径穿越（`../../../etc/passwd`）时返回错误
- [ ] `job.cancelled` 调用后 job.status 为 Cancelled（不是 Failed）
- [ ] `subtask.create` 无权限时返回 Unauthorized
- [ ] `subtask.create` 有权限时 job 的 depends_on 包含 caller 的 job_id

### 参考文档

- `INTERFACES.md` §MCP Tool 签名
- `MCP_CONTRACT.md` §Worker 默认工具集
- `MCP_CONTRACT.md` §Worker 初始 Context
- `CONTEXT_MANAGEMENT.md` §层 1：Worker 自主 Checkpoint
- `OPEN_QUESTIONS.md` Q16（job.request 回路）
- `OPEN_QUESTIONS.md` Q21（git + checkpoint auto-commit）

---

## M5：Process Manager

### 目标

实现 provider 进程的启动、追踪、终止。实现 `kingdom up` 和 `kingdom down` 的核心流程。
完成本 milestone 后，可以真正在 tmux 里启动一个 Claude/Codex 进程并让它连接 Kingdom MCP。

### 实现范围

**Provider 发现（`src/process/discovery.rs`）：**

```rust
pub struct ProviderDiscovery;

impl ProviderDiscovery {
    // 检测已安装的 provider，返回可用列表
    pub fn detect(config: &KingdomConfig) -> Vec<DetectedProvider>;
    // 检测单个 provider 的 binary 是否可用
    pub fn check(provider: &str, config: &KingdomConfig) -> Option<PathBuf>;
    // 检测对应 API key 环境变量是否已设置
    pub fn check_api_key(provider: &str) -> bool;
}

pub struct DetectedProvider {
    pub name: String,
    pub binary: PathBuf,
    pub api_key_set: bool,
}
```

内置 API key 环境变量映射：
- `claude` → `ANTHROPIC_API_KEY`
- `codex` → `OPENAI_API_KEY`
- `gemini` → `GEMINI_API_KEY`

**Provider 启动（`src/process/launcher.rs`）：**

```rust
pub struct ProcessLauncher {
    workspace_path: PathBuf,
    config: KingdomConfig,
}

impl ProcessLauncher {
    // 启动 provider 进程，返回 PID 和 tmux pane_id
    pub async fn launch(
        &self,
        provider: &str,
        role: WorkerRole,
        worker_id: &str,
        job_id: Option<&str>,
        initial_context: Option<&str>,
    ) -> Result<LaunchResult>;

    // 终止进程（graceful: SIGTERM + 5s → SIGKILL）
    pub async fn terminate(&self, pid: u32, graceful: bool) -> Result<()>;
}

pub struct LaunchResult {
    pub pid: u32,
    pub pane_id: String,
}
```

**Provider Adapter（`src/process/adapter.rs`）：**

不同 CLI 在启动参数、工作目录、退出码、首次交互行为上有差异，统一由 adapter 兜底，launcher 不直接调用裸模板。

```rust
pub trait ProviderAdapter: Send + Sync {
    // 生成完整启动命令（替换占位符后的最终 args）
    fn build_args(&self, mcp_config_path: &Path, role: WorkerRole) -> Vec<String>;
    // provider 进程的工作目录（None = 继承 Kingdom 工作目录）
    fn working_dir(&self, workspace_path: &Path) -> Option<PathBuf>;
    // 判断退出码是否为"正常退出"（不触发 failover）
    fn is_clean_exit(&self, code: i32) -> bool;
    // MCP 连接就绪前的等待时间（provider 首次启动可能有初始化延迟）
    fn connection_grace_period(&self) -> Duration;
}

pub struct ClaudeAdapter;
pub struct CodexAdapter;
pub struct GeminiAdapter;
pub struct CustomAdapter { args_template: Vec<String> }
```

**三个内置 adapter 的已知差异：**

| 属性 | claude | codex | gemini |
|---|---|---|---|
| MCP config 参数 | `--mcp-config {path}` | `--mcp-config {path}` | 待确认 |
| 工作目录 | 继承 | 继承 | 继承 |
| 正常退出码 | 0 | 0 | 0 |
| 连接等待 | 3s | 5s | 5s |
| 首次启动特殊行为 | 可能弹 auth 提示 | 可能需要 login | 待确认 |

**注意**：表中"待确认"的字段在 M5 联调时填入真实值，联调前以保守值（5s 等待、任意退出码视为崩溃）为默认。如 adapter 行为在联调中与文档不符，以代码实现为准，回来更新此表。

**启动流程：**
1. 根据 `provider` 名选择对应 adapter
2. 生成 MCP config 文件（基于 `worker.json` 模板，注入 job_id）
3. 执行 `tmux split-window -h -P -F "#{pane_id}"` 获取 pane_id
4. 在 pane 里执行：`adapter.build_args()` 返回的完整命令，工作目录由 `adapter.working_dir()` 指定
5. 在 pane 顶部注入提示行：`[Kingdom] 直接在此输入不会被记录，仅用于紧急干预`
6. 记录 PID（通过 `tmux display-message -p "#{pane_pid}"` 获取）
7. 等待 `adapter.connection_grace_period()`，超时仍未握手则视为启动失败

**Worker.create 工具实现（补充 M3 的骨架）：**

```rust
// 实际启动逻辑
pub async fn worker_create(provider: &str) -> Result<WorkerId> {
    // 1. 检查 provider 已安装
    // 2. 生成 worker_id（w{seq}）
    // 3. 选择 pane 位置（主 window ≤3 worker 用 split-window，否则新建 window）
    // 4. 调用 ProcessLauncher::launch
    // 5. 更新 Session，写 state.json
    // 6. 写 action log
}
```

**Pane 布局策略：**
- 主 window 第 1 个 worker：`tmux split-window -h`（左右分割）
- 主 window 第 2-3 个 worker：`tmux split-window -v`（上下分割）
- 超过 3 个：`tmux new-window -n "kingdom:w{n}"`

**Daemon PID 文件：**

Daemon 启动时将自身 PID 写入 `.kingdom/daemon.pid`，格式为纯数字加换行。Kingdom CLI（`kingdom down` / `kingdom restart`）通过读取此文件获取 daemon PID：
```rust
// daemon main() 启动时
let pid = std::process::id();
std::fs::write(storage.root.join("daemon.pid"), format!("{}\n", pid))?;
```

**Watchdog（`crates/kingdom-watchdog/src/main.rs`）：**

按技术选型节的设计实现。额外要求：
- 接收 `SIGTERM` 时向 daemon 发 `SIGTERM`，等待 daemon 退出后自己退出
- watchdog PID 写入 `.kingdom/watchdog.pid`

**Kingdom up（`src/cli/up.rs`）：**

```
1. 检查 tmux 是否安装
2. 检查 git（没有则警告，询问是否以 strategy=none 继续）
3. 检测已有 session（检查 socket + state.json）：
   - daemon + session 都在 → 提示 kingdom attach，退出
   - daemon 在，session 丢了 → 重建 tmux session，跳到步骤 8
   - 全新启动 → 继续
4. Storage::init（创建 .kingdom/ 目录 + .gitignore）
5. ProviderDiscovery::detect，输出可用 provider 列表和 API key 状态
6. 如果 manager provider 不可用 → 报错退出
7. 生成 MCP config（manager.json + worker.json）
8. 启动 Kingdom MCP server（后台 daemon）
9. 询问默认 manager provider（只列可用的）
10. 创建 tmux session（session_name 从 config.toml 读取）
11. 在 pane-0 启动 manager，注入 manager system prompt
12. 等待 manager MCP 连接（超时 15s）
13. 输出：✓ 启动完成
```

**Manager System Prompt 注入：**

Kingdom 在 manager 启动时通过 MCP 工具 `workspace.status()` 的结果自动构造初始 context 消息，格式：

```
你是 Kingdom 的 manager。
当前 workspace：{workspace_path}
可用 worker provider：{available_providers}
当前状态：{workspace.status 快照}

你的职责：分析用户意图、拆分任务、派发给 worker、审查结果。
通过 MCP 工具与 Kingdom 交互，不要直接操作文件系统。

{KINGDOM.md 内容（如果存在）}
```

**Kingdom down（`src/cli/down.rs`）：**

```
1. 检查是否有 running job
2. 有 → 询问：[等待完成] [暂停并退出] [强制退出]
   - 等待完成：轮询直到所有 job 完成
   - 暂停并退出：向每个 running worker 发 checkpoint 请求（10s 窗口），超时强制退出
   - 强制退出：直接 SIGKILL
3. 无 → 直接退出
4. 按序终止：worker → manager → MCP server → watchdog
5. 清理 socket 文件
```

**Config 热加载（`src/config/watcher.rs`）：**

Daemon 启动后开启一个后台任务，每 5 秒检测 `.kingdom/config.toml` 的修改时间（`metadata().modified()`）。若变化则 reload，并发送 `SIGHUP` 给自身触发 handler：

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

**注意：** 以下配置字段修改后立刻生效：`idle.timeout_minutes`、`health.*`、`notifications.*`。以下字段需要 `kingdom restart` 才生效（影响已启动进程）：`tmux.session_name`、`providers.*`。

**KINGDOM.md 模板（`kingdom up` 首次运行）：**

若 workspace 根目录没有 `KINGDOM.md` 且 `KINGDOM.md.example` 也没有，`kingdom up` 初始化时询问：

```
未找到 KINGDOM.md。是否生成模板？[Y/n]
```

若 Y，根据 workspace 中检测到的语言/框架生成模板：

```markdown
# Kingdom 工作约束

## 代码规范
- 语言：{检测结果，如 Rust / TypeScript / Python}
- 禁止：{如 unwrap()、any、print debugging}

## 架构约束
- （在此描述不能改动的架构决策）

## 风格偏好
- （在此描述 AI 应遵守的代码风格）
```

语言检测：读取根目录的 `Cargo.toml` / `package.json` / `pyproject.toml` / `go.mod`，取第一个匹配。

**Idle 超时（`src/process/idle_monitor.rs`）：**

```rust
// 后台任务，每分钟检查一次
pub async fn idle_monitor(session: Arc<Mutex<Session>>) {
    // 找所有 status=Idle 且 idle 超过 config.idle.timeout_minutes 的 worker
    // 调用 ProcessLauncher::terminate
    // 更新 worker.status → Terminated
    // 写 action log
}
```

### 不实现

- 健康监控（M6）
- Failover（M7）
- tmux status bar 更新（M8）
- `kingdom log / doctor / clean`（M9）

### 验收条件

- [ ] `cargo test` 全部通过（进程相关测试可用 mock）
- [ ] `kingdom up` 在新目录成功启动，创建 `.kingdom/` 和 tmux session
- [ ] `kingdom up` 检测到 manager provider 不可用时报错退出
- [ ] `kingdom up` 检测到已有 session 时提示 attach
- [ ] `kingdom up` 在非 git 目录时警告并询问
- [ ] `worker.create` 启动进程并记录 PID
- [ ] `worker.create` 第 4 个 worker 在新 tmux window 里启动
- [ ] `kingdom down` 无运行中 job 时直接退出
- [ ] `kingdom down` 有运行中 job 时显示三个选项
- [ ] Idle 超时后 worker 进程被终止，status 更新为 Terminated
- [ ] `kingdom down --force` 跳过询问直接终止所有进程
- [ ] Daemon 启动后 `.kingdom/daemon.pid` 写入当前 daemon 进程 PID
- [ ] `config.toml` 修改后 5 秒内 daemon 自动 reload，无需 restart（`idle.timeout_minutes` 变化即时生效）
- [ ] `kingdom up` 未找到 `KINGDOM.md` 时询问生成模板
- [ ] Watchdog 启动后 `.kingdom/watchdog.pid` 存在
- [ ] Kill daemon 进程后 watchdog 在 1 秒内重启它
- [ ] `kingdom down` 后 watchdog 正常退出（不循环重启）

### 参考文档

- `ARCHITECTURE.md` §Provider 发现
- `ARCHITECTURE.md` §Provider 启动流程
- `ARCHITECTURE.md` §Kingdom 启动顺序
- `ARCHITECTURE.md` §MCP Config 结构
- `ARCHITECTURE.md` §Manager 初始 Prompt
- `UX.md` §Tmux 布局
- `UX.md` §关停 UX
- `OPEN_QUESTIONS.md` Q7（Provider 发现）
- `OPEN_QUESTIONS.md` Q10（Bootstrap 细节）
- `OPEN_QUESTIONS.md` Q11（kingdom down）
- `OPEN_QUESTIONS.md` Q19（并发 worker 上限）
- `OPEN_QUESTIONS.md` Q28（worker idle 复用）
- `OPEN_QUESTIONS.md` Q35（Provider 启动参数）
- `OPEN_QUESTIONS.md` Q41（非 git 目录）

---

## M6：健康监控

### 目标

实现 Kingdom 对每个 provider 的持续健康监控。检测到异常时触发 `HealthEvent`，为 M7 的 failover 状态机提供输入。
本 milestone 只负责**检测和发事件**，不负责处理（处理在 M7）。

### 实现范围

**健康监控器（`src/health/monitor.rs`）：**

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

    // 启动后台监控循环
    pub async fn run(&self);
}
```

**四个监控维度：**

**1. 心跳监控**

```rust
// 每 heartbeat_interval_seconds 秒检查一次
// 计算每个 connected worker 上次 heartbeat 的时间差
// 超过 interval * timeout_count 秒未收到 → 触发事件
HealthEvent::HeartbeatMissed {
    worker_id,
    consecutive_count,  // 连续未响应次数
}
```

- 只监控 `mcp_connected = true` 的 worker
- `context.ping` 的处理（M4 已实现）负责更新 `last_heartbeat`，本模块只读取时间戳

**2. 进程存活监控**

```rust
// 每 5 秒轮询一次（比心跳更频繁，进程退出需要快速响应）
// 检查 worker.pid 对应的进程是否还在
// 通过 /proc/{pid}/status（Linux）或 kill -0 {pid}（跨平台）检测
// 进程不存在 → 触发事件
HealthEvent::ProcessExited {
    worker_id,
    exit_code,  // 从 waitpid 获取，无法获取时用 -1
}
```

- 进程退出**立即触发**，不等心跳超时

**3. Context 阈值监控**

```rust
// context.ping 更新 worker.context_usage_pct 时（M4 已触发 HealthEvent）
// HealthMonitor 消费这些事件，决定是否发送 checkpoint 请求
// 阈值：50% / 70% / 85% / 90%
// 90% 直接加入 failover 候选，不发 checkpoint 请求
```

**发送 checkpoint 请求的流程（见 INTERFACES.md §context.checkpoint_request 协议）：**

```rust
// 1. Kingdom 向 worker 发送 kingdom.checkpoint_request notification（无 id，单向推送）：
//    { "method": "kingdom.checkpoint_request", "params": { "job_id": "job_001", "urgency": "Normal" } }
// 2. 等待（按 urgency 的延迟窗口）：
//    - worker 调用 job.checkpoint()          → 成功，重置 context threshold 计数
//    - worker 调用 context.checkpoint_defer() → 记录延迟，更新下次触发时间
//    - 超过延迟窗口未响应                    → 降级：Kingdom 生成 fallback checkpoint
// 3. 同一 worker 的等待中状态防止重复发送（幂等）
```

延迟窗口（与 CheckpointUrgency 对应）：
- Normal (≥50%)：最多延迟 60 秒
- High (≥70%)：最多延迟 15 秒
- Critical (≥85%)：不允许延迟；`context.checkpoint_defer()` 返回 `ValidationFailed`

**4. Progress 超时监控**

```rust
// 每分钟检查一次
// 找所有 status=Running 且 last_progress 超过 progress_timeout_minutes 的 worker
// 触发事件（不是 failover，只是警告）
HealthEvent::ProgressTimeout {
    worker_id,
    elapsed_minutes,
}
```

**Rate limit 处理（`src/health/rate_limiter.rs`）：**

```rust
// 当 MCP 工具调用返回 rate limit 错误时触发
// 不进入 failover，单独处理：指数退避重试
// 退避序列：5s → 15s → 30s → 60s（封顶）
// 重试 3 次仍失败 → 触发 HealthEvent::ProcessExited（降级为崩溃处理）

pub struct RateLimitHandler {
    retry_counts: HashMap<String, u32>,  // worker_id → retry count
}

impl RateLimitHandler {
    pub async fn handle(&mut self, worker_id: &str) -> RateLimitResult;
}

pub enum RateLimitResult {
    Retrying { wait_secs: u64 },
    Exhausted,  // 3次后降级
}
```

**降级 checkpoint（`src/health/fallback_checkpoint.rs`）：**

```rust
// 当 worker 未在窗口内响应 checkpoint 请求时使用
// 内容：只有 git diff，没有文字摘要（五项留空但标注"[自动生成，无摘要]"）
pub async fn generate_fallback_checkpoint(
    job_id: &str,
    workspace_path: &Path,
) -> CheckpointContent;
```

### 不实现

- 响应 HealthEvent（failover 触发在 M7）
- 进度超时的 popup 显示（M8）
- Rate limit 的 status bar 更新（M8）

### 验收条件

- [ ] `cargo test` 全部通过（使用 mock 进程和 mock 时间）
- [ ] 心跳：worker 60 秒未更新 `last_heartbeat` 触发 `HeartbeatMissed { consecutive_count: 2 }`
- [ ] 进程：kill worker 进程后 5 秒内触发 `ProcessExited`
- [ ] Context 50%：Kingdom 发出 `kingdom.checkpoint_request` notification，urgency=Normal
- [ ] Context 85%：Kingdom 发出 `kingdom.checkpoint_request` notification，urgency=Critical
- [ ] Context 85% + worker 调用 `context.checkpoint_defer()`：返回 ValidationFailed
- [ ] Context Normal 60s 内 worker 未响应：Kingdom 生成降级 fallback checkpoint
- [ ] 同一 worker 等待 checkpoint 期间：Kingdom 不重复发送 `kingdom.checkpoint_request`（幂等）
- [ ] Worker 收到 `kingdom.checkpoint_request` 后再收到 `kingdom.cancel_job`：优先处理取消，不需要先 checkpoint
- [ ] Progress timeout：30 分钟无 `job.progress` 触发 `ProgressTimeout`
- [ ] Rate limit：连续 3 次后触发降级 failover
- [ ] 所有 HealthEvent 正确写入 action log

### 参考文档

- `ARCHITECTURE.md` §健康监控
- `INTERFACES.md` §context.checkpoint_request 协议（kingdom.checkpoint_request notification 完整规范）
- `CONTEXT_MANAGEMENT.md` §层 1：Worker 自主 Checkpoint
- `FAILOVER.md` §检测
- `OPEN_QUESTIONS.md` Q13（Failover 误触发防护）
- `OPEN_QUESTIONS.md` Q33（progress 超时值）

---

## M7：Failover 状态机

### 目标

实现完整的 failover 流程：从 HealthEvent 触发，到新 provider 接手工作。
包含熔断机制、手动 swap、rate limit 降级、manager failover。

### 实现范围

**Failover 状态机（`src/failover/machine.rs`）：**

```rust
pub struct FailoverMachine {
    session: Arc<Mutex<Session>>,
    config: FailoverConfig,
    health_rx: mpsc::Receiver<HealthEvent>,
    launcher: ProcessLauncher,
    tmux: TmuxController,        // M8 之前用 stub
}

impl FailoverMachine {
    pub async fn run(&self);     // 消费 HealthEvent，驱动状态机
}
```

**事件优先级与冲突处理（单一入口规则）：**

所有状态变更必须经过 `FailoverMachine::run` 的 event loop，禁止在其他地方直接修改 worker/job 状态。多个事件同时到达时按以下优先级处理：

| 优先级 | 事件类型 | 说明 |
|---|---|---|
| 1（最高）| `ProcessExited` | 进程已死，确定性事件，立即处理 |
| 2 | `ManualSwap` | 用户主动触发，不计熔断 |
| 3 | `ContextLimit`（≥90%）| context 即将耗尽，需立刻切换 |
| 4 | `HeartbeatTimeout` | 心跳超时，卡死但未退出 |
| 5（最低）| `ProgressTimeout` | 长时间无上报，只警告不切换 |

**冲突场景的明确规则：**

| 场景 | 规则 |
|---|---|
| `HeartbeatTimeout` 和 `ProgressTimeout` 同时触发 | 按优先级 4 处理 HeartbeatTimeout，ProgressTimeout 事件丢弃 |
| rate limit 重试中收到 `ManualSwap` | 中止重试，立刻执行 ManualSwap（优先级 2 > rate limit handler） |
| job 处于 `Cancelling` 时 provider crash | 忽略 failover，job 直接改为 `Cancelled`，不启动新 provider |
| 熔断窗口内既有自动 failover 又有 ManualSwap | ManualSwap 不受熔断限制（不计入失败次数，不受冷却约束） |
| 新 provider 连接超时又收到第二个 HealthEvent | 当前 failover 流程未完成，第二个事件放入队列等待 |

worker 处于 failover 流程中（`failover_in_progress = true`）时，新到的 HealthEvent 进入队列缓存，当前流程完成（成功或 Paused）后再处理队列。

**完整 failover 流程：**

```
HealthEvent 触发
  ↓
1. 熔断检查
   10 分钟内同一 job 失败 ≥ 3 次 → job 标记 Paused，发 ManagerNotification，停止
   两次 failover 间隔 < 30 秒 → 冷却等待后继续
  ↓
2. 判断是否用户手动停止（5 秒缓冲窗口）
   ProcessExited 时弹 popup（M8 之前写日志代替）：
   "是你手动停止的吗？[是，暂停任务] [否，触发切换]"
   - 5 秒无响应 → 按崩溃处理，继续
   - 用户选"是" → job 标记 Paused，停止
  ↓
3. 准备 handoff brief
   - 取最近一个 checkpoint 内容
   - 用 git diff 计算 possibly_incomplete_files（崩溃前最后修改的文件）
   - 构造 HandoffBrief
  ↓
4. 确认切换（弹 popup，M8 之前写日志代替）
   显示：失败原因 + handoff brief 摘要 + 推荐 provider
   [确认切换] [选择其他] [取消]
   - 取消 → job 标记 Paused，停止
  ↓
5. 启动新 provider
   - 同一个 pane（tmux respawn-pane）
   - 传入 HandoffBrief 作为初始 context
  ↓
6. 等待新 provider MCP 连接（15 秒超时）
   超时 → 显示警告，提供 [重试] [换其他] [暂停]
  ↓
7. 写 HANDOFF 分隔线到 pane（M8 之前写日志代替）
8. 更新 Session：新 worker_id，job 继续 Running
9. 写 action log
```

**熔断机制（`src/failover/circuit_breaker.rs`）：**

```rust
pub struct CircuitBreaker {
    failure_records: HashMap<String, Vec<DateTime<Utc>>>,  // job_id → 失败时间列表
    config: FailoverConfig,
}

impl CircuitBreaker {
    pub fn record_failure(&mut self, job_id: &str) -> CircuitBreakerResult;
    pub fn check_cooldown(&self, worker_id: &str) -> Option<Duration>;
}

pub enum CircuitBreakerResult {
    Ok,
    Tripped,   // 触发熔断
}
```

**Provider 稳定性历史（`src/failover/stability.rs`）：**

在 `state.json` 中为每个 provider 维护当次 session 的崩溃/超时计数，failover 推荐时将其纳入权重：

```rust
pub struct ProviderStability {
    pub provider: String,
    pub crash_count: u32,        // ProcessExited 触发的 failover 次数
    pub timeout_count: u32,      // HeartbeatTimeout 触发的 failover 次数
    pub last_failure_at: Option<DateTime<Utc>>,
}
```

更新时机：每次 failover 完成后，将 failed_provider 的对应计数 +1，写入 `state.json`。

**推荐逻辑扩展：** 决策表步骤 7 改为按稳定性分排序：`crash_count + timeout_count` 最少的候选优先，同分时按 `claude > codex > gemini` 排。

**推荐 provider 逻辑（`src/failover/recommender.rs`）：**

```rust
pub fn recommend_provider(
    failed_provider: &str,
    available_providers: &[String],   // 只包含已探测到的可用 provider（未安装的不在此列）
    failure_reason: &FailoverReason,
    session_failures: &[String],      // 本次 failover 链中已经失败过的 provider，避免再推荐
    manager_provider: &str,           // manager 正在使用的 provider，不推荐
    stability: &HashMap<String, ProviderStability>,  // 本次 session 稳定性历史
) -> Option<String>;
```

**决策表（确定性，可写单元测试）：**

| 步骤 | 条件 | 动作 |
|---|---|---|
| 1 | 候选集 = available_providers | 初始候选 |
| 2 | 排除 failed_provider | 从候选集移除 |
| 3 | 排除 session_failures 中的所有项 | 从候选集移除 |
| 4 | 排除 manager_provider | 从候选集移除 |
| 5 | 候选集为空 | 返回 `None` |
| 6 | `failure_reason == ContextLimit` | 候选集中有 claude → 返回 claude |
| 7 | 其他原因 | 按优先级表顺序返回第一个：claude > codex > gemini |

**测试用例（必须全部实现）：**
- `recommend("codex", ["claude","codex","gemini"], Crash, [], "claude")` → `Some("gemini")`（claude 是 manager，排除）
- `recommend("codex", ["claude","codex"], ContextLimit, [], "gemini")` → `Some("claude")`（ContextLimit 优先 claude）
- `recommend("claude", ["claude"], Crash, [], "n/a")` → `None`（候选集排除后为空）
- `recommend("codex", ["claude","codex"], Crash, ["claude"], "gemini")` → `None`（session_failures 排完后无候选）

**Manager Failover 特殊处理：**

```rust
// manager 失败时，所有 running worker 暂停接收新任务
// 走同一套 failover 流程，但新 manager 的初始 context 不同：
pub fn build_manager_recovery_context(session: &Session) -> String {
    // 包含：KINGDOM.md + workspace.notes + 所有 job 状态 + 最近 N 条 action log
    // 不依赖对话历史（Q27）
}
```

**手动 Swap（`src/cli/swap.rs`）：**

```
kingdom swap {worker_id} [provider]
  ↓
1. 验证 worker_id 存在且不是 manager
2. 向 worker 发 checkpoint 请求（urgency=High，10 秒窗口）
3. 超时 → 用 git diff 生成降级 checkpoint
4. 弹确认 popup（M8 之前打印到 stdout）
5. 确认 → 走标准 failover 流程，reason=Manual
6. Manual failover 不计入熔断计数
```

**`job.cancel` 两阶段 shutdown（补充 M3）：**

```rust
// Phase 1：Kingdom 向 worker 发送 MCP notification：
//   {"jsonrpc":"2.0","method":"kingdom.cancel_job","params":{"job_id":"job_001","reason":"manager_cancelled"}}
// 等待 30 秒
// Phase 2a：worker 调用 job.cancelled() → 干净停止 → job.status = Cancelled，git stash 改动
// Phase 2b：30 秒超时 → SIGKILL → job.status = Cancelled，git stash 尽力保存
```

两个阶段的最终状态都是 `Cancelled`，不是 `Failed`。

### 不实现

- tmux popup 显示（M8）：本 milestone 用 stdout 打印替代
- tmux HANDOFF 分隔线（M8）：本 milestone 用日志替代
- status bar 更新（M8）

### 验收条件

- [ ] `cargo test` 全部通过
- [ ] ProcessExit → failover 触发 → 新 provider 启动（集成测试用 mock provider）
- [ ] HeartbeatTimeout → failover 触发
- [ ] ContextLimit（90%）→ failover 触发
- [ ] 10 分钟内同一 job 3 次失败 → 熔断，job 标记 Paused
- [ ] 两次 failover 间隔 < 30 秒 → 冷却后继续
- [ ] 新 provider 15 秒内未连接 → 显示超时警告
- [ ] `kingdom swap w1` 发 checkpoint 请求，10 秒超时后降级
- [ ] Manual swap 不计入熔断计数
- [ ] Manager failover 后新 manager 收到完整恢复 context（含 job 状态 + notes）
- [ ] `job.cancel` graceful stop：worker 30 秒内调用 `job.cancelled()` 完成
- [ ] `job.cancel` graceful stop 超时：SIGKILL 后 git stash
- [ ] `recommend_provider` 决策表四个测试用例全部通过（见 BUILD_PLAN §推荐 provider 逻辑）
- [ ] `HeartbeatTimeout` + `ProgressTimeout` 同时到达：只处理 HeartbeatTimeout，ProgressTimeout 丢弃
- [ ] rate limit 重试中收到 `ManualSwap`：中止重试，立刻执行切换
- [ ] job 处于 `Cancelling` 时 provider crash：job 改为 `Cancelled`，不触发 failover
- [ ] ManualSwap 不受熔断限制（熔断窗口内 ManualSwap 仍可执行）
- [ ] failover 进行中收到第二个 HealthEvent：事件入队，当前流程完成后处理
- [ ] provider 崩溃 2 次后，`recommend_provider` 优先推荐崩溃次数更少的候选
- [ ] `state.json` 中 `provider_stability` 字段在每次 failover 后正确更新

### 参考文档

- `FAILOVER.md` 全文
- `CONTEXT_MANAGEMENT.md` §切换时的交接简报
- `INTERFACES.md` §MCP 协议定义（`kingdom.cancel_job` notification、最终状态语义）
- `OPEN_QUESTIONS.md` Q17（failover 时文件破损）
- `OPEN_QUESTIONS.md` Q21（git 策略与 failover）
- `OPEN_QUESTIONS.md` Q24（provider 断线重连）
- `OPEN_QUESTIONS.md` Q27（manager context 超限）
- `OPEN_QUESTIONS.md` Q8（kingdom swap）

---

## M8：tmux 集成

### 目标

实现所有 tmux 可见层：status bar、popup 确认框、HANDOFF 分隔线、session 恢复 UX、ManagerNotification 推送。
完成本 milestone 后，用户看到的交互体验完整。

### 实现范围

**tmux 控制器（`src/tmux/controller.rs`）：**

```rust
pub struct TmuxController {
    session_name: String,
}

impl TmuxController {
    // Status bar
    pub fn update_status_bar(&self, session: &Session) -> Result<()>;

    // Popup
    pub fn show_popup(&self, popup: &Popup) -> Result<PopupResult>;

    // Pane 操作
    pub fn inject_line(&self, pane_id: &str, line: &str) -> Result<()>;
    pub fn respawn_pane(&self, pane_id: &str, command: &str) -> Result<()>;

    // Session 管理
    pub fn create_session(&self) -> Result<()>;
    pub fn session_exists(&self) -> bool;
    pub fn attach(&self) -> Result<()>;
}
```

**Status Bar（`src/tmux/status_bar.rs`）：**

格式：`[{provider}:{role_abbr}{status_icon}] ... {cost}  {time}`

```rust
pub fn render_status_bar(session: &Session) -> String;
```

规则：
- Manager：`[Claude:mgr]`
- Worker running：`[Codex:w1]`
- Worker completed：`[Codex:w1✓]`（3 秒后恢复无图标）
- Worker attention：`[Gemini:w2⚠]`
- Worker failed：`[Codex:w3✗]`
- Worker failover：`[Codex:w1↻]`
- Worker rate limited：`[Codex:w1⏳]`
- Idle worker：`[idle]`

更新时机（通过事件触发，不轮询）：
- worker status 变化
- job 完成/失败
- failover 开始/完成
- context_usage_pct 变化（每 10% 更新一次）

```bash
tmux set-option -g status-right "{rendered_string}"
tmux refresh-client -S
```

**Popup（`src/tmux/popup.rs`）：**

```rust
pub struct Popup {
    pub title: String,
    pub body: String,
    pub options: Vec<PopupOption>,
    pub timeout_secs: Option<u32>,
    pub default_on_timeout: Option<usize>,  // 超时后选第几个选项（0-indexed）
}

pub struct PopupOption {
    pub label: String,
    pub key: char,               // 快捷键
}

pub enum PopupResult {
    Selected(usize),
    Timeout,
    Dismissed,
}

impl TmuxController {
    pub fn show_popup(&self, popup: &Popup) -> Result<PopupResult> {
        // 生成临时 shell 脚本，通过 tmux display-popup 展示
        // 等待用户输入后返回结果
    }
}
```

**Popup 超时规则：**
- Failover 确认 popup：无超时（等用户）
- 进程退出 5 秒缓冲 popup：5 秒超时，默认"否，触发切换"
- Progress timeout 警告 popup：无超时

**tmux 命令失败降级策略：**

tmux 命令可能因版本差异、pane 已关闭、session 丢失等原因失败。每类操作的降级规则：

| 操作 | 失败时降级 |
|---|---|
| `display-popup`（需 tmux ≥ 3.2）| 降级：向 manager pane 发 `send-keys` 文本提示，写 action log 要求用户手动执行 |
| `inject_line` 向 pane 注入文本 | 降级：跳过注入，写 action log，不阻塞主流程 |
| `update status bar`（`set-option`）| 降级：静默跳过，下次尝试时重试 |
| `respawn-pane`（failover 重启 provider）| 不能降级，失败则报错给 manager，job 标记 Paused |
| `tmux new-window`（4+ worker）| 降级：报错提示用户手动创建 window，不阻断 worker.create 的状态写入 |

**版本检测：** `kingdom up` 时记录 tmux 版本到 session state，`display-popup` 前检查，低版本直接走降级路径，不尝试后失败。

**`display-popup` 降级的文本格式（注入 manager pane）：**
```
[Kingdom 需要确认] job_001 的 worker Codex 崩溃
  原因: HeartbeatTimeout
  操作: failover.confirm("w1", "claude") 切换 | failover.cancel("w1") 暂停
```

**HANDOFF 分隔线（`src/tmux/handoff.rs`）：**

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
         原因: {}\n\
         已传递: {}\n\
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

**Failover 空窗期 UX：**

```rust
// 旧 provider 停止到新 provider 连接期间，pane 里显示计时：
// ⏳ 正在启动 Claude... (3s)
// 每秒更新，新 provider 连接后停止
pub async fn show_startup_progress(
    pane_id: &str,
    provider: &str,
    tmux: &TmuxController,
    connected: watch::Receiver<bool>,
) -> Result<()>;
```

**Manager Notification 推送（`src/mcp/notifier.rs`）：**

```rust
pub struct ManagerNotifier {
    manager_connection: Option<Arc<McpConnection>>,
    notification_mode: NotificationMode,
    pending_queue: VecDeque<ManagerNotification>,
}

impl ManagerNotifier {
    // Push 模式：发送 `kingdom.event` notification 给 manager
    // method: "kingdom.event"
    // params: { "type": "<event_type>", "data": <event_data>, "text": "<格式化文本>" }
    // text 字段是预格式化的对话消息，provider 直接注入 context
    pub async fn push(&self, notification: &ManagerNotification) -> Result<()>;

    // Manager 断连时：加入队列；重连 kingdom.hello 时通过 queued_notifications 批量下发
    // queued_notifications 格式与 push 相同（kingdom.event params 数组）
    pub async fn flush_queue(&self) -> Result<()>;

    // 格式化 notification 为 manager 对话消息文本（中文，见 MCP_CONTRACT.md §Notification 格式）
    pub fn format_notification(notification: &ManagerNotification) -> String;
}

// kingdom.event params 示例：
// {
//   "type": "job_completed",
//   "data": { "job_id": "job_001", "worker_id": "w1", "changed_files": [...] },
//   "text": "[Kingdom] job_001 已完成\n  worker: w1 (Codex)\n  摘要: ...\n  → 调用 job.result(...)"
// }
```

Notification 消息格式示例（注入 manager 对话流）：

```
[Kingdom] job_001 已完成
  worker: w1 (Codex)
  摘要: 实现了登录验证，修改了 3 个文件
  changed: src/auth/LoginForm.tsx, src/auth/validation.ts
  → 调用 job.result("job_001") 查看完整结果
```

**Session 恢复 UX（补充 M5 的 kingdom up）：**

```
$ kingdom up

检测到上次 session 有未完成工作：

  job_001  实现登录验证          [running → w1 已暂停]
  job_002  写前端调用登录接口     [waiting → 依赖 job_001]
  job_003  添加单元测试           [completed ✓]

workspace.notes:
  · 用 TypeScript，禁止 any
  · src/auth/ 不引入新依赖

继续上次工作？[Y/n]
```

**kingdom attach（`src/cli/attach.rs`）：**

```bash
tmux attach-session -t {session_name}
```

**系统通知（`src/notifications/system.rs`）：**

```rust
pub fn send_notification(title: &str, body: &str, level: NotificationLevel) -> Result<()> {
    match level {
        NotificationLevel::Bell => print!("\x07"),  // terminal bell
        NotificationLevel::System => {
            // macOS: osascript -e 'display notification "{body}" with title "{title}"'
            // Linux: notify-send "{title}" "{body}"
            // 平台不支持时降级为 Bell
        }
        NotificationLevel::None => {}
    }
}
```

### 不实现

- M9 的 CLI 命令（log / doctor / clean / cost）

### 验收条件

- [ ] `cargo test` 全部通过（tmux 命令用 mock）
- [ ] Status bar 格式正确渲染（单元测试 `render_status_bar`）
- [ ] Worker 状态变化后 status bar 在 1 秒内更新
- [ ] Failover popup 正确显示三个选项
- [ ] 5 秒缓冲 popup 超时后自动选"否，触发切换"
- [ ] HANDOFF 分隔线格式正确，包含时间戳和原因
- [ ] Failover 空窗期计时每秒更新
- [ ] Manager notification 以正确格式注入对话流
- [ ] Manager 断连时 notification 排队，重连后推送
- [ ] Session 恢复时显示正确的 job 状态摘要
- [ ] Bell notification 触发终端响铃
- [ ] `kingdom attach` 连接到已有 tmux session
- [ ] `display-popup` 失败（mock 版本过低）时降级为 manager pane 文本注入
- [ ] `inject_line` 失败时静默跳过，不阻塞主流程，写 action log
- [ ] `update_status_bar` 失败时静默跳过，不 panic
- [ ] Status bar 超过 4 个 worker 时简化为 `[w1:✓] [w2:⚡] [w3:✓] [+N]` 格式，不溢出
- [ ] Manager context 耗尽触发 failover 时，旧 manager pane 注入静态标记行：`[Kingdom] 此 manager 已被新 manager 接手，请切换到 manager pane`；status bar 中旧 pane 标记为 `[manager:stale]`
- [ ] Kingdom 向 manager 注入 job notification 时，对 result_summary 内容做基础过滤：截断连续超过 2KB 的内容，检测并警告包含 `system:` / `<system>` 前缀的可疑指令块（写 action log，不阻止注入）

### 参考文档

- `UX.md` 全文
- `FAILOVER.md` §Failover 空窗期 UX
- `MCP_CONTRACT.md` §Manager 对话模型
- `MCP_CONTRACT.md` §Kingdom → Manager 推送事件
- `OPEN_QUESTIONS.md` Q3（popup 超时）——见本 milestone 的超时规则
- `OPEN_QUESTIONS.md` Q14（worker pane 直接打字）
- `OPEN_QUESTIONS.md` Q20（离开时通知）
- `OPEN_QUESTIONS.md` Q25（manager notification 机制）

---

## M9：CLI 命令

### 目标

实现所有辅助 CLI 命令：`log`、`doctor`、`clean`、`cost`、`restart`。
这些命令读取 `.kingdom/` 状态，不修改运行时。

### 实现范围

**`kingdom log`（`src/cli/log.rs`）：**

三个视图，互斥：

```
kingdom log                        # 默认：job 列表
kingdom log --detail <job_id>      # 单个 job 完整时间线
kingdom log --actions [--limit N]  # 原始操作流
```

**默认视图：**

```
job_003  ✓  添加单元测试              completed  14:21  Codex(w3)  3m12s
job_002  ✓  写前端调用登录接口        completed  13:45  Gemini(w2) 8m04s
job_001  ✓  实现登录验证              completed  12:58  Codex(w1)  22m31s
            ↳ failover: Codex→Claude 14:02 (context 超限)
```

- 从 `state.json` 读取（所有 job 元数据在 state.json，无独立 meta.json）
- 按 `created_at` 倒序排列
- 耗时 = `completed_at - created_at`
- failover 记录从 action log 中提取

**`--detail` 视图：**

```
job_001  实现登录验证
  created    12:58  by manager
  worker     Codex w1（12:58 → 14:02）
  failover   14:02  context 超限 → Claude w4
  worker     Claude w4（14:02 → 15:20）
  completed  15:20  3 files changed
  branch     kingdom/job_001

  checkpoints:
    13:15  [kingdom checkpoint] 验证逻辑完成，表单提交进行中
    13:19  [kingdom checkpoint] 表单提交完成，待写测试
```

**`--actions` 视图：**

```
14:21  manager(w0)  job.complete   job_003
14:20  worker(w3)   job.progress   job_003  "测试全部通过"
14:02  kingdom      failover       job_001  Codex→Claude
13:15  worker(w1)   job.checkpoint job_001
```

---

**`kingdom doctor`（`src/cli/doctor.rs`）：**

五层检查，逐层输出。Kingdom 未运行时只做系统层和配置层检查。

```
检查 Kingdom 运行环境...

[系统依赖]
✓ tmux 3.3a
✓ git 2.42
✗ codex    未安装  → npm install -g @openai/codex
✓ claude   已安装

[API Key]
✓ ANTHROPIC_API_KEY    已设置
✗ OPENAI_API_KEY       未设置  → export OPENAI_API_KEY=sk-...

[Kingdom Daemon]
✓ daemon 运行中  PID 12345  已运行 2h34m
✓ MCP socket    /tmp/kingdom/a3f9c2.sock
✓ watchdog      运行中  PID 12346

[当前 Session]
✓ manager    Claude   已连接  context 23%
⚠ w1        Codex    心跳超时 45s  → kingdom swap w1
✓ w2        Gemini   已连接  context 41%

[配置文件]
✓ .kingdom/config.toml    有效
✓ KINGDOM.md              存在
⚠ .kingdom/manager.json  MCP socket 路径过时  → kingdom restart
```

实现细节：
- 系统依赖：`which tmux`, `which git`, `which {provider}` + version
- API Key：检查环境变量是否存在（不验证有效性）
- Daemon：检查 socket 文件 + PID 存活
- Session：从 state.json 读取，检查每个 worker 的 `last_heartbeat`
- 配置文件：尝试解析 config.toml，检查 manager.json 的 socket 路径是否与当前一致

---

**`kingdom clean`（`src/cli/clean.rs`）：**

```
kingdom clean              # 显示预览，询问确认
kingdom clean --dry-run    # 只显示，不执行
kingdom clean --all        # 不按时间限制，清理所有可清理内容
```

清理项目（按 ARCHITECTURE.md 保留策略）：
- 已完成 job 中间 checkpoint（保留最后一个，删除 7 天前的其余 checkpoint）
- 已完成 job 最终结果（90 天后归档到 `.kingdom/archive/`）
- action log 旧条目（30 天前的压缩为摘要行：`[compressed] {date}: {n} actions`）

输出格式：

```
将清理以下内容：

  已完成 job 中间 checkpoint（>7天）
    job_001  3 个 checkpoint  · 2.1 MB  2026-03-15
    job_002  5 个 checkpoint  · 4.8 MB  2026-03-18

  归档已完成 job 结果（>90天）
    job_047  · 1.2 MB  2025-12-10

  压缩旧 action log（>30天）
    2026-02-01 ~ 2026-03-05  · 18 MB → 约 0.5 MB

合计释放：约 26 MB

[确认清理]  [取消]
```

---

**`kingdom cost`（`src/cli/cost.rs`）：**

```
今日花费：$0.34
  Claude   128k tokens   $0.19  ████████░░
  Codex     89k tokens   $0.11  █████░░░░░
  Gemini    45k tokens   $0.04  ██░░░░░░░░

本周：$2.17  本月：$8.43

最贵的 job：job_003（实现登录验证）$0.18
```

内置单价（可在 config.toml 覆盖）：

```toml
[cost]
claude_input_per_1m = 3.00      # USD
claude_output_per_1m = 15.00
codex_input_per_1m = 2.50
codex_output_per_1m = 10.00
gemini_input_per_1m = 0.075
gemini_output_per_1m = 0.30
```

数据来源：action log 中每条 `context.ping` 记录的 token_count 差值。

**数据口径约定（三个命令共用，写入 action log 时必须遵守）：**

| 字段 | 口径 |
|---|---|
| token 计数 | `context.ping` 写入的累计值；压缩后的 action log 保留每个 job 的最后一条 ping（含累计 token），cost 计算从 job 的 first_ping 到 last_ping 差值 |
| 耗时 | `job.completed_at - job.created_at`，未完成 job 用 `now() - created_at` 估算 |
| failover 归属 | failover 期间的 token 计入**新** provider（旧 provider crash 前的消耗已记录在旧 worker 的 context.ping 里）|
| clean 后的 cost | action log 压缩行（`[compressed] {date}: {n} actions, tokens: {total}`）中保留 token 汇总，cost 可从压缩行还原 |
| doctor 修复命令跨平台 | 只输出 shell 命令（sh/bash 语法），不输出 PowerShell；macOS 和 Linux 使用相同命令格式；`export VAR=...` 而非 `set VAR=...` |

---

**`kingdom restart`（`src/cli/restart.rs`）：**

```
1. 向 daemon 发送 SIGTERM
2. 等待最多 5 秒
3. 超时 → SIGKILL
4. 重新启动 daemon（从 watchdog 触发或直接启动）
5. Daemon 从 .kingdom/ 恢复状态
6. 重连所有存活 provider（15 秒超时）
7. 输出：✓ daemon 已重启，session 恢复正常
```

注意：tmux session 和 provider 进程全程不中断。

---

**`kingdom swap`（补充 M7 的实现，完整版）：**

```
kingdom swap {worker_id}              # 弹出 provider 选择列表
kingdom swap {worker_id} {provider}   # 直接指定
```

provider 选择列表只显示 `available_providers` 中已安装的。

---

**`kingdom replay <job_id>`（`src/cli/replay.rs`）：**

读取 `job.intent`，用相同 intent 创建新 job 并直接派发：

```
kingdom replay job_001
  ↓
1. 读取 job_001 的 intent
2. 调用 job.create(intent)（生成 job_002）
3. 如有可用 idle worker，询问：立刻分配？[Y/n]
4. Y → worker.assign(idle_worker, job_002)
5. 输出：✓ 重新创建 job_002，intent：{intent[:60]}
```

适用场景：job 失败后想用相同 intent 重跑，无需手动复制粘贴。

---

**`kingdom job diff <job_id>`（`src/cli/job_diff.rs`）：**

显示某个 job 从开始到完成的 git diff：

```
kingdom job diff job_001
  ↓
git diff {job.branch_start_commit}..HEAD -- {job.changed_files}
```

`git strategy = None` 时返回提示："该 job 在非 git 模式下运行，无 diff 记录。"
已归档的 job（>90 天）仍可查看（commit hash 已记录，diff 历史由 git 保留）。

---

**`kingdom open <worker_id_or_job_id>`（`src/cli/open.rs`）：**

跳转到关联的 tmux pane：

```
kingdom open w1      # 跳转到 w1 的 pane
kingdom open job_001 # 跳转到正在执行 job_001 的 worker pane
```

实现：读取 `worker.pane_id` 或通过 `job.worker_id` 找到 pane_id，调用 `tmux select-pane -t {pane_id}`。
若 pane 已不存在：输出提示 "pane 已关闭（job 已结束）。"

---

**Notification Webhook（`src/notifications/webhook.rs`）：**

config.toml 配置：

```toml
[notifications]
on_attention_required = "bell"    # 已有字段

[notifications.webhook]
url = "https://hooks.slack.com/..."  # 可选
events = ["job.completed", "job.failed", "failover.triggered"]  # 订阅事件
timeout_seconds = 5
```

payload 格式（HTTP POST，Content-Type: application/json）：

```json
{
  "event": "job.completed",
  "job_id": "job_001",
  "worker": "Codex(w1)",
  "summary": "实现了登录验证，修改了 3 个文件",
  "workspace": "/path/to/repo",
  "timestamp": "2026-04-06T10:30:00Z"
}
```

webhook 调用失败时静默跳过（超时/5xx），写 action log 警告，不阻塞主流程。

### 验收条件

- [ ] `kingdom log` 默认视图按时间倒序，failover 记录正确显示
- [ ] `kingdom log --detail job_001` 显示完整时间线和 checkpoint 列表
- [ ] `kingdom log --actions` 从 action.jsonl 正确解析并格式化
- [ ] `kingdom doctor` 在 daemon 未运行时只检查系统层和配置层
- [ ] `kingdom doctor` 检测到心跳超时 worker 时显示 `→ kingdom swap w1`
- [ ] `kingdom clean --dry-run` 不修改任何文件
- [ ] `kingdom clean` 确认后正确删除过期 checkpoint
- [ ] `kingdom clean` 压缩 action log 后原始条目删除，摘要行写入
- [ ] `kingdom cost` 从 action log 计算 token 使用量，格式正确
- [ ] `kingdom cost` action log 压缩后仍能从压缩行的 token 汇总正确计算费用
- [ ] `kingdom clean` 压缩后的 action log 压缩行包含 token 汇总字段
- [ ] `kingdom doctor` 修复命令使用 `export VAR=...` 语法，不含平台特有语法
- [ ] `kingdom restart` daemon 重启后 provider 进程 PID 不变
- [ ] `kingdom swap w1` 不传 provider 时显示可用 provider 列表
- [ ] `kingdom replay job_001` 创建新 job 并附带原始 intent
- [ ] `kingdom job diff job_001` 输出正确的 git diff（测试用临时 git repo）
- [ ] `kingdom job diff` 在 `git strategy = None` 时输出友好提示
- [ ] `kingdom open w1` 调用 `tmux select-pane` 并跳转（mock tmux 验证调用参数）
- [ ] webhook 配置了 url 时，job.completed 事件触发 HTTP POST
- [ ] webhook 超时（5s）时静默跳过，不阻塞 job 完成流程
- [ ] webhook 调用失败写 action log 警告条目

### 参考文档

- `UX.md` §`kingdom log` 输出格式
- `UX.md` §`kingdom doctor` 诊断输出
- `UX.md` §用户交互入口
- `ARCHITECTURE.md` §存储管理
- `OPEN_QUESTIONS.md` Q23（kingdom doctor）
- `OPEN_QUESTIONS.md` Q31（kingdom log 格式）
- `OPEN_QUESTIONS.md` Q32（kingdom restart）
- `OPEN_QUESTIONS.md` Q37（kingdom clean）

---

## M10：端到端集成

### 目标

把 M1-M9 的所有组件串联起来，跑通完整场景，修复集成时发现的问题。
本 milestone **不新增功能**，只做集成、修复、补边界情况。

### 测试场景

每个场景需要有自动化集成测试（允许使用 mock provider，但 tmux 操作必须真实执行）。

---

**场景 1：Happy Path（单 worker）**

```
1. kingdom up（选择 claude 作为 manager）
2. Manager 调用 job.create("实现一个简单函数")
3. Manager 调用 worker.create("codex")
4. Manager 调用 worker.assign(w1, job_001)
5. Worker(w1) 调用 job.progress("开始实现")
6. Worker(w1) 调用 job.complete("已实现，修改了 foo.rs")
7. Manager 收到 notification，调用 job.result(job_001)
8. kingdom log 显示 job_001 completed
9. kingdom down
```

验收：
- [ ] 全流程无错误
- [ ] state.json 中 job_001 status=Completed
- [ ] action.jsonl 包含所有步骤的记录
- [ ] tmux status bar 在每步正确更新
- [ ] `kingdom log` 显示正确的耗时和 worker 信息

---

**场景 2：Happy Path（并行 worker）**

```
1. kingdom up
2. 创建 job_001, job_002, job_003（互相独立）
3. 同时启动 3 个 worker 分别执行
4. 三个 job 按不同顺序完成
5. Manager 依次 review 结果
6. kingdom down
```

验收：
- [ ] 3 个 worker 在主 window 里 2x2 布局正确
- [ ] 三个 branch 独立（kingdom/job_001, job_002, job_003）
- [ ] 各 worker 的 context.ping 独立追踪

---

**场景 3：Job 依赖链**

```
1. job_001（无依赖）
2. job_002（depends_on=[job_001]）
3. job_003（depends_on=[job_001, job_002]）
4. job_001 完成 → job_002 变 Pending，manager 收到 notification
5. job_002 完成 → job_003 变 Pending
6. job_001 取消 → manager 收到 cascade 警告
```

验收：
- [ ] 依赖未满足时 job 保持 Waiting
- [ ] 依赖满足后 ManagerNotification::JobUnblocked 正确触发
- [ ] job.cancel 有级联时写入警告日志

---

**场景 4：Failover（context 超限）**

```
1. Worker 执行任务，context.ping 上报 pct=0.91
2. Kingdom 触发 failover
3. Worker 先做 checkpoint（urgency=Critical）
4. Popup 显示：失败原因 + handoff brief 摘要
5. 用户确认切换
6. 同一 pane 启动新 provider
7. HANDOFF 分隔线出现在 pane 里
8. 新 provider 收到 handoff brief，继续工作
9. 新 provider 完成 job
```

验收：
- [ ] Checkpoint 五项内容完整
- [ ] possibly_incomplete_files 正确识别
- [ ] 新 provider 在同一 branch 继续
- [ ] HANDOFF 分隔线格式正确
- [ ] 熔断计数递增

---

**场景 5：Session 恢复**

```
1. kingdom up，启动 job_001（running）
2. kingdom down --force（强制退出，不等 checkpoint）
3. kingdom up
4. 显示恢复摘要（job_001 paused）
5. 用户确认继续
6. Manager 重启，收到恢复 context
7. Manager 决定 resume job_001
```

验收：
- [ ] state.json 在 down 后保留 job 状态
- [ ] workspace.notes 跨 session 保留
- [ ] 恢复摘要格式正确
- [ ] Manager 收到完整恢复 context（含 notes + job 状态）

---

**场景 6：Kingdom daemon 崩溃恢复**

```
1. kingdom up，有 running worker
2. 直接 kill Kingdom daemon（SIGKILL）
3. Watchdog 在 1 秒内重启 daemon
4. Provider 自动重连（指数退避）
5. Daemon 重连后 session 状态恢复
6. 用户无感知，工作继续
```

验收：
- [ ] Watchdog 在 2 秒内完成重启
- [ ] Provider 重连后 MCP 工具调用恢复正常
- [ ] 重连期间缓存的 tool call 在恢复后补报
- [ ] status bar 在重连后恢复显示

---

**场景 7：边界情况**

```
7a. 非 git 目录：kingdom up 警告并降级 strategy=none
7b. API key 缺失：kingdom up 警告但继续（worker provider 缺失）
7c. Provider binary 不存在：worker.create 失败，给出安装提示
7d. job.complete 幂等：连续调用两次不报错
7e. worker.release 对 Running worker：返回 InvalidState 错误
7f. file.read 路径穿越：返回错误
7g. 超出 3 个 worker：第 4 个在新 tmux window
7h. kingdom up 发现已有 session：提示 attach
```

验收：每个边界情况有对应测试，行为与 OPEN_QUESTIONS.md 中的决策一致。

---

### 代码结构检查

M10 完成时，检查以下项目：

- [ ] `src/` 目录结构与模块划分合理（`types/`, `storage/`, `mcp/`, `process/`, `health/`, `failover/`, `tmux/`, `cli/`）
- [ ] 无 `unwrap()` / `expect()` 在非测试代码中（全部换成 `?` 或明确错误处理）
- [ ] 无未使用的 `#[allow(dead_code)]`
- [ ] `cargo clippy` 无 warning
- [ ] `cargo test` 全部通过（单测 + 集成测试）
- [ ] 所有写操作经过 Storage 层，无直接 `std::fs::write` 散落在业务代码中

### 最终验收

- [ ] 场景 1-7 全部通过
- [ ] `kingdom doctor` 在干净环境下输出全绿
- [ ] `kingdom log` 正确显示场景 4 的 failover 记录
- [ ] `kingdom clean --dry-run` 在测试数据上输出正确的清理预览
- [ ] OPEN_QUESTIONS.md 中所有 Q1-Q44 的决策在代码中均有对应实现

### 参考文档

- 所有设计文档（最终集成时以设计文档为准）
- `OPEN_QUESTIONS.md` 全文（逐条对照验证）
- `INTERFACES.md` §ID 格式规范（最终验证 ID 格式一致性）

---

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| Eng Review | `/plan-eng-review` | Architecture & tests (required) | 1 | passed-with-changes | 8 issues found, 8 resolved, 1 deferred (distribution/CI) |
| CEO Review | `/plan-ceo-review` | Scope & strategy | 1 | passed-with-changes | SELECTIVE_EXPANSION; 8 proposals, 7 accepted, 1 deferred (headless) |
| Codex Review | `/codex review` | Independent 2nd opinion | 0 | — | — |
| Design Review | `/plan-design-review` | UI/UX gaps | 0 | — | — |

**ACCEPTED SCOPE (CEO review):** config hot-reload (M5), KINGDOM.md template (M5), provider stability history (M7), `kingdom replay` (M9), `kingdom job diff` (M9), `kingdom open` (M9), notification webhook (M9), status bar overflow handling (M8), manager stale pane UX (M8), prompt injection warning (M8)

**CRITICAL GAP (mitigated):** Prompt injection via result_summary → manager context. Mitigated in M8 with basic content filtering + action log warning.

**DEFERRED:** Headless mode (P3, TODOS.md), Distribution/CI pipeline (P2, TODOS.md)

**VERDICT:** ENG + CEO REVIEWS PASSED — ready to implement M1. Run `/plan-design-review` for deep tmux/TUI UX review before M8 implementation.
