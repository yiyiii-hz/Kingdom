# M9a：只读 / 诊断 CLI

## 目标

实现四个只读或纯本地操作的 CLI 命令：`log`、`doctor`、`cost`、`clean`。
这些命令不连接运行中的 daemon，直接读写 `.kingdom/` 目录。

---

## 前置条件

M1–M8 全部完成。本 milestone 涉及以下**修改**：

| 文件 | 类型 | 原因 |
|---|---|---|
| `src/config/mod.rs` | 修改 | 新增 `CostConfig` |
| `src/storage.rs` | 修改 | 新增 archive / clean 方法 |
| `src/mcp/tools/worker/context.rs` | 修改 | context.ping 写 action log |
| `src/cli/mod.rs` | 修改 | 注册新模块 |
| `src/bin/kingdom.rs` | 修改 | 注册新子命令 |

以及以下**新建**：

```
src/cli/log.rs
src/cli/doctor.rs
src/cli/cost.rs
src/cli/clean.rs
```

---

## 1. Config 扩展（`src/config/mod.rs`）

在 `KingdomConfig` 中增加 `cost` 字段：

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KingdomConfig {
    // ... 现有字段不变 ...
    #[serde(default)]
    pub cost: CostConfig,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CostConfig {
    pub claude_input_per_1m: f64,
    pub claude_output_per_1m: f64,
    pub codex_input_per_1m: f64,
    pub codex_output_per_1m: f64,
    pub gemini_input_per_1m: f64,
    pub gemini_output_per_1m: f64,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            claude_input_per_1m: 3.00,
            claude_output_per_1m: 15.00,
            codex_input_per_1m: 2.50,
            codex_output_per_1m: 10.00,
            gemini_input_per_1m: 0.075,
            gemini_output_per_1m: 0.30,
        }
    }
}
```

`CostConfig` 对应 `config.toml` 中的 `[cost]` section，全部字段可选（缺失时用默认值）。

**单元测试：** `cost_config_defaults` 验证默认值正确。

---

## 2. context.ping 写 action log（`src/mcp/tools/worker/context.rs`）

在 `ContextPingTool::call` 成功保存 session 后，追加一条 action log 记录：

```rust
append_action_log(
    &self.storage,
    caller,
    self.name(),   // "context.ping"
    json!({
        "worker_id": worker_id,
        "job_id": worker.job_id,   // Option<String>，可为 null
        "token_count": params.token_count,
        "usage_pct": params.usage_pct,
    }),
    None,
)?;
```

**要求：**
- 追加在 `save_session` 之后、返回 `Ok(Value::Null)` 之前
- action log 格式与其他工具一致（`actor` 来自 caller，`action` = `"context.ping"`）
- 已有的 `context_ping_*` 测试必须继续通过；同时新增一个测试验证 action log 被写入

---

## 3. Storage 扩展（`src/storage.rs`）

新增以下方法，**不修改**现有方法签名：

### 3a. `list_checkpoint_files`

```rust
/// 返回某个 job 的所有 checkpoint 文件路径，按文件名（即 checkpoint_id）升序。
pub fn list_checkpoint_files(&self, job_id: &str) -> Result<Vec<PathBuf>>;
```

实现：`glob` `.kingdom/jobs/{job_id}/checkpoints/*.json`，按文件名排序返回。

### 3b. `archive_job`

```rust
/// 将 job 的 result.json 移动到 .kingdom/archive/{job_id}/result.json。
/// checkpoint 文件和 handoff.md 保留原位（不移动）。
/// 移动前创建目标目录。
pub fn archive_job(&self, job_id: &str) -> Result<()>;
```

目标路径：`{storage.root}/archive/{job_id}/result.json`

### 3c. `compress_action_log`

```rust
/// 将 action log 中 cutoff 之前的条目压缩为摘要行，保留 cutoff 之后的条目。
///
/// 压缩行格式：写入一条合法的 ActionLogEntry，字段如下：
///   actor:  "kingdom"
///   action: "compressed_summary"
///   params: {
///     "date_from": "<ISO8601>",
///     "date_to": "<ISO8601>",
///     "count": <n>,
///     "tokens": <cumulative_tokens>   // 压缩区间内所有 context.ping 的最大 token_count 之和（按 worker 分组）
///   }
///   timestamp: cutoff
///
/// 操作：
///   1. 读取整个 action.jsonl
///   2. 按 cutoff 分割为 old / new
///   3. 若 old 为空，直接返回 Ok(())
///   4. 计算 tokens = sum_over_workers(max token_count in old 区间)
///   5. 用 write_atomically 将 [compressed_entry] + new_entries 写回 action.jsonl
pub fn compress_action_log(
    &self,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<()>;
```

**注意：** `read_action_log` 现有实现不变，`compressed_summary` 条目是合法的 `ActionLogEntry`，会被正常返回；`cost` 和 `clean` 命令自行识别并处理它们。

### 3d. `delete_old_checkpoints`

```rust
/// 删除 job 所有 checkpoint 中，除最后一个之外、创建时间早于 cutoff 的 checkpoint 文件。
/// 返回实际删除的文件数。
pub fn delete_old_checkpoints(
    &self,
    job_id: &str,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<usize>;
```

实现：
1. `list_checkpoint_files(job_id)` 获取文件列表（已按名升序，最后一个是最新）
2. 逐个读取 JSON 解析 `created_at`
3. 跳过最后一个文件（`files.last()`）
4. 对其余文件：若 `created_at < cutoff`，删除

**单元测试要求：**
- `test_archive_job` —— 调用后 result.json 出现在 archive 路径，原路径消失
- `test_compress_action_log` —— 压缩后文件只剩 1 条 compressed_summary + N 条新条目
- `test_delete_old_checkpoints_keeps_last` —— 最后一个 checkpoint 永不删除

---

## 4. `kingdom log`（`src/cli/log.rs`）

### 函数签名

```rust
pub fn run_log(
    workspace: PathBuf,
    detail: Option<String>,   // --detail <job_id>
    actions: bool,            // --actions
    limit: Option<usize>,     // --limit N（仅 --actions 模式有效）
) -> Result<(), Box<dyn std::error::Error>>;
```

### 4a. 默认视图（既无 `--detail` 也无 `--actions`）

从 `state.json` 读取所有 job，**按 `created_at` 倒序**输出：

```
job_003  ✓  添加单元测试              completed  14:21  Codex(w3)  3m12s
job_002  ✓  写前端调用登录接口        completed  13:45  Gemini(w2) 8m04s
job_001  ✓  实现登录验证              completed  12:58  Codex(w1)  22m31s
            ↳ failover: Codex→Claude  14:02  (context 超限)
```

字段说明：
- 状态图标：`✓`=Completed，`✗`=Failed，`⚠`=Paused，`⏳`=Running/Pending/Waiting，`-`=Cancelled
- intent 截断到 24 字符（中文算 2 宽度，用 `unicode_width` crate 或简单按字节 / 字符近似）
- 完成时间：`result.completed_at` 格式化为 `HH:MM`；未完成的 job 显示 `--:--`
- Worker：`{Provider}({worker_id})`，无 worker 时显示 `-`
- 耗时：`result.completed_at - job.created_at`；未完成用 `now() - created_at` 并加 `~` 前缀
- failover 行：从 action log 中提取 `action == "failover.start"` 的条目，`params` 含 `job_id`、`from_provider`、`to_provider`、`reason`

**Provider 名称格式：** 首字母大写（同 status_bar 的 `capitalize` 函数，可复用）。

### 4b. `--detail <job_id>` 视图

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

数据来源：
- job 元数据来自 `state.json`（`session.jobs`）
- checkpoint 列表来自 `job.checkpoints`（`Vec<CheckpointMeta>`），每条读取对应的 `CheckpointContent`（从文件）以获取 `done` 摘要
- failover 行：从 action log 提取该 job_id 的 `failover.start` 条目
- branch：`job.branch`
- `3 files changed`：`job.result.changed_files.len()`

若 `job_id` 不存在，输出 `"job not found: {job_id}"` 并以非零退出。

### 4c. `--actions [--limit N]` 视图

```
14:21  manager(w0)  job.complete   job_003
14:20  worker(w3)   job.progress   job_003  "测试全部通过"
14:02  kingdom      failover       job_001  Codex→Claude
13:15  worker(w1)   job.checkpoint job_001
```

从 `action.jsonl` 读取全部（或最后 N 条），格式：
```
{HH:MM}  {actor:<12}  {action:<16}  {params_summary}
```

`params_summary` 规则：
- 若 `params` 有 `job_id` 字段，先显示 `job_id`
- 若 action 为 `"context.ping"`，显示 `token: {token_count}`
- 若 `params` 有 `reason`，追加 `reason`
- 其余字段截断到 40 字符

特殊处理：`action == "compressed_summary"` 时输出：
```
{date_from} ~ {date_to}  [compressed: {count} actions, {tokens} tokens]
```

**单元测试：**
- `test_log_default_shows_jobs_sorted` —— 多个 job 时按 created_at 倒序，状态图标正确
- `test_log_detail_missing_job` —— 输出 "job not found"
- `test_log_actions_respects_limit` —— `--limit 2` 只输出最后 2 条

---

## 5. `kingdom doctor`（`src/cli/doctor.rs`）

### 函数签名

```rust
pub fn run_doctor(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>>;
```

### 检查层次

输出格式见下方，每项用 `✓` / `✗` / `⚠` 开头。

#### [系统依赖]

检查以下工具是否安装（`which {tool}` 返回 0）及版本：
- `tmux`：`tmux -V` → `tmux 3.3a`
- `git`：`git --version` → `git 2.42.0`
- `codex`：`which codex`（无 version 命令，显示 `已安装` / `未安装`）
- `claude`：`claude --version` 或 `which claude`
- `gemini`：`which gemini`

未安装时显示修复建议：
- codex：`→ npm install -g @openai/codex`
- claude：`→ npm install -g @anthropic-ai/claude-code`（或 `pip install claude-code`，显示两种）
- gemini：`→ 参考 https://ai.google.dev/gemini-api/docs/quickstart`

#### [API Key]

检查以下环境变量是否存在（非空）：
- `ANTHROPIC_API_KEY`
- `OPENAI_API_KEY`
- `GOOGLE_API_KEY`（或 `GEMINI_API_KEY`，两者均检查，任一有效则 ✓）

未设置时显示：`→ export {VAR}=<your-key>`（只用 `export` 语法，不用平台特有语法）

#### [Kingdom Daemon]

仅在 `.kingdom/daemon.pid` 存在时检查：
- PID 是否存活（`kill(pid, 0)`）
- 若存活：显示 `daemon 运行中  PID {pid}  已运行 {duration}`（duration 从 pid 文件 mtime 计算）
- MCP socket：`/tmp/kingdom/{workspace_hash}.sock` 是否存在
- Watchdog：`.kingdom/watchdog.pid` 是否存在且进程存活

未运行时跳过本层（输出 `[Kingdom Daemon] 未运行，跳过`）。

#### [当前 Session]

仅在 daemon 运行时且 `state.json` 存在时检查。从 `state.json` 读取：
- Manager worker：显示 provider、连接状态、context usage
- 每个 Worker：provider、连接状态（`mcp_connected`）、心跳超时检测

心跳超时判断：`Utc::now() - worker.last_heartbeat > health_config.heartbeat_interval_seconds * heartbeat_timeout_count`

超时时显示：`⚠ {worker_id}  {provider}  心跳超时 {elapsed}s  → kingdom swap {worker_id}`

#### [配置文件]

- 尝试 parse `.kingdom/config.toml`（若存在）：成功 ✓，失败 ✗ 显示错误位置
- 检查 `KINGDOM.md` 是否存在于 workspace 根目录
- 检查 MCP socket 路径：若 daemon 运行中但 socket 文件不存在，显示 `⚠ socket 丢失 → kingdom restart`

**示例完整输出：**

```
检查 Kingdom 运行环境...

[系统依赖]
✓ tmux 3.3a
✓ git 2.42.0
✗ codex    未安装  → npm install -g @openai/codex
✓ claude   已安装

[API Key]
✓ ANTHROPIC_API_KEY    已设置
✗ OPENAI_API_KEY       未设置  → export OPENAI_API_KEY=<your-key>

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
```

**单元测试：**
- `test_doctor_export_syntax` —— 修复命令中不含 `set ` 或 `$env:` 等平台特有语法
- `test_doctor_heartbeat_threshold` —— 超时判断逻辑（传入 mock worker 和 health config）

---

## 6. `kingdom cost`（`src/cli/cost.rs`）

### 函数签名

```rust
pub fn run_cost(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>>;
```

### Token 数据来源

扫描 `action.jsonl` 中所有 `action == "context.ping"` 条目：

```jsonc
// 每条 context.ping 的 params:
{
  "worker_id": "w1",
  "job_id": "job_001",   // nullable
  "token_count": 12345,  // 累计值（单调递增，per worker）
  "usage_pct": 0.45
}
```

对每个 worker，找到该 worker 的**最大 token_count**（即该 worker 生命周期内的总 token 消耗）。

同时处理 `action == "compressed_summary"` 条目（`params.tokens` 字段），作为历史 token 总量加入计算。

**Failover 归属规则：** token 归属于写入该条目时的 worker（即 `actor` 字段标识的 worker），failover 期间新 worker 自行累计。

### Worker → Provider 映射

从 `state.json` 的 `session.workers` 获取 `worker.provider`。若 session 不存在（已清理），从 action log 的 `worker.create` 条目中推断。

### 费用计算

```
// 简化：不区分 input/output，使用 (input_per_1m + output_per_1m) / 2 作为均价
cost_usd = token_count / 1_000_000.0 * avg_price_per_1m
```

时间分组（用 `context.ping` 的 timestamp）：
- 今日：`timestamp.date() == today`
- 本周：`timestamp.iso_week() == this_week`
- 本月：`timestamp.month() == this_month`

最贵的 job：对 job_id 分组，累计该 job 所有 worker 的 token 消耗，排序取最大。

### 输出格式

```
今日花费：$0.34
  Claude   128k tokens   $0.19  ████████░░
  Codex     89k tokens   $0.11  █████░░░░░
  Gemini    45k tokens   $0.04  ██░░░░░░░░

本周：$2.17  本月：$8.43

最贵的 job：job_003（实现登录验证）$0.18
```

进度条：共 10 格，按该 provider 占今日总费用的比例填充 `█`，其余 `░`。

若无任何 context.ping 数据，输出 `暂无费用数据（context.ping 尚未写入 action log）`。

**单元测试：**
- `test_cost_calculates_from_context_ping` —— 构造 action log 条目，验证计算结果
- `test_cost_handles_compressed_summary` —— compressed_summary 中的 tokens 被加入总量

---

## 7. `kingdom clean`（`src/cli/clean.rs`）

### 函数签名

```rust
pub fn run_clean(
    workspace: PathBuf,
    dry_run: bool,
    all: bool,
) -> Result<(), Box<dyn std::error::Error>>;
```

### 清理规则

使用 `Utc::now()` 作为基准：

| 清理项 | 条件 | 操作 |
|---|---|---|
| 已完成 job 中间 checkpoint | 非最后一个 + `created_at < now - 7天`（`all` 时跳过 7 天限制） | 删除文件 |
| 已完成 job 最终结果 | `result.completed_at < now - 90天`（`all` 时跳过 90 天限制） | 移动到 `.kingdom/archive/` |
| action log 旧条目 | `timestamp < now - 30天`（`all` 时跳过 30 天限制） | 调用 `compress_action_log` |

"已完成 job"：`job.status == Completed`

### 输出格式（预览 + 确认）

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

继续清理？[y/N]
```

- `--dry-run`：输出上述内容后不询问，直接退出（退出码 0，不修改任何文件）
- `--all`：跳过时间限制，清理所有可清理内容（仍需确认，除非还加了 `--dry-run`）
- 若无任何可清理内容：输出 `没有需要清理的内容。` 并退出

文件大小计算：用 `std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)` 逐文件求和。

### 执行顺序

1. delete_old_checkpoints（每个 completed job）
2. archive_job（每个超期 completed job）
3. compress_action_log（cutoff = now - 30天）

**单元测试：**
- `test_clean_dry_run_does_not_modify` —— dry_run=true 时文件不变
- `test_clean_archives_old_jobs` —— 超期 job 的 result.json 被移动到 archive
- `test_clean_compresses_old_action_log` —— 30 天前的条目被压缩

---

## 8. CLI 注册

### `src/cli/mod.rs`

新增：
```rust
pub mod clean;
pub mod cost;
pub mod doctor;
pub mod log;
```

### `src/bin/kingdom.rs`

新增子命令：

```rust
enum Commands {
    // ... 现有 ...
    Log {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        detail: Option<String>,
        #[arg(long)]
        actions: bool,
        #[arg(long)]
        limit: Option<usize>,
    },
    Doctor {
        #[arg(default_value = ".")]
        workspace: PathBuf,
    },
    Cost {
        #[arg(default_value = ".")]
        workspace: PathBuf,
    },
    Clean {
        #[arg(default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        all: bool,
    },
}
```

在 `main()` 中分别调用对应的 `run_*` 函数。

---

## 验收条件

- [ ] `cargo test` 全部通过
- [ ] `cargo clippy` 无 error

### Config
- [ ] `CostConfig::default()` 六个字段值正确

### context.ping
- [ ] context.ping 调用后 action log 有对应条目（含 token_count 字段）
- [ ] 已有 context_ping_* 测试继续通过

### Storage
- [ ] `archive_job` 后 result.json 出现在 archive 路径，原路径不存在
- [ ] `compress_action_log` 后文件只含 1 条 compressed_summary + cutoff 之后的条目
- [ ] `compress_action_log` 对空 old 区间直接返回，不写文件
- [ ] `delete_old_checkpoints` 最后一个 checkpoint 永不删除
- [ ] `delete_old_checkpoints` 返回实际删除数

### kingdom log
- [ ] 默认视图按 created_at 倒序，状态图标正确（✓ ✗ ⚠ ⏳ -）
- [ ] failover 行从 action log 正确提取（含箭头格式 `Codex→Claude`）
- [ ] `--detail` 显示 checkpoints 列表（从文件读取 `done` 字段）
- [ ] `--detail` 对不存在的 job_id 输出错误并非零退出
- [ ] `--actions --limit 5` 只输出最后 5 条
- [ ] compressed_summary 条目显示为 `[compressed: N actions, T tokens]`

### kingdom doctor
- [ ] 修复命令使用 `export VAR=...` 语法，不含 `set ` / `$env:` 等
- [ ] daemon 未运行时跳过 [Daemon] 和 [Session] 检查层
- [ ] 心跳超时 worker 显示 `→ kingdom swap {worker_id}`

### kingdom cost
- [ ] 有 context.ping 数据时计算结果非零
- [ ] compressed_summary 的 tokens 被纳入计算
- [ ] 无数据时输出友好提示
- [ ] 进度条 10 格，按比例填充

### kingdom clean
- [ ] `--dry-run` 不修改任何文件
- [ ] 无可清理内容时输出提示并正常退出
- [ ] 超期 job 结果被 archive（`archive_job` 被调用）
- [ ] action log 被压缩（`compress_action_log` 被调用）

---

## 参考文档

- `BUILD_PLAN.md` §M9（权威规格）
- `UX.md` §`kingdom log` 输出格式
- `UX.md` §`kingdom doctor` 诊断输出
- `ARCHITECTURE.md` §存储管理（保留策略）
