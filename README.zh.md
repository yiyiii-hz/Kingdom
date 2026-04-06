# Kingdom v2

> English version: [README.en.md](./README.en.md)

原生于终端的 AI worker 编排。多个 provider，一个会话，自动故障切换。

```
┌──────────────────────┬──────────────────────────────────────┐
│  管理者             │  worker-1                            │
│  [Claude]            │  [Codex]                             │
│                      │                                      │
│  正在分发任务…       │  正在实现认证中间件…                  │
│                      │                                      │
├──────────────────────┼──────────────────────────────────────┤
│  worker-2            │  worker-3                            │
│  [Gemini]            │  [空闲]                              │
│                      │                                      │
│  正在编写 CSS…       │  等待任务                             │
│                      │                                      │
├──────────────────────┴──────────────────────────────────────┤
│ [Claude:mgr] [Codex:impl✓] [Gemini:ui⚠] [idle]  $0.34  14:32 │
└─────────────────────────────────────────────────────────────┘
```

当 provider 失败时，Kingdom 会继续让工作保持运行。当 Codex 命中 context 上限或掉线时，Kingdom 会检测到故障，请求确认，然后在同一个 pane 内带着压缩后的 briefing 把任务交接给 Claude。工作会继续推进，不需要手动重建。

---

## 需求

- **Rust** 1.75+
- **tmux** 3.0+
- **git** 2.30+
- 至少安装并完成认证的一个 AI provider CLI：
  - [claude](https://github.com/anthropics/anthropic-quickstarts) (`ANTHROPIC_API_KEY`)
  - [codex](https://github.com/openai/codex) (`OPENAI_API_KEY`)
  - [gemini](https://ai.google.dev/) (`GEMINI_API_KEY`)

## 安装

```bash
git clone https://github.com/your-org/kingdom-v2
cd kingdom-v2
cargo build --release
cargo build --release -p kingdom-watchdog
cargo build --release -p kingdom-bridge
cp target/release/kingdom ~/.local/bin/
cp target/release/kingdom-watchdog ~/.local/bin/
cp target/release/kingdom-bridge ~/.local/bin/
```

> **重要：** `kingdom`、`kingdom-watchdog` 和 `kingdom-bridge` 必须放在**同一个目录**里。运行时，`kingdom-watchdog` 会通过它自己的所在目录去定位 `kingdom`，而 `kingdom` daemon 也会用同样的方式定位 `kingdom-bridge`。如果把它们放在不同位置，会导致启动报错。

验证：

```bash
kingdom --help
kingdom doctor
```

`kingdom doctor` 会检查你的环境，并明确告诉你缺了什么。

---

## 快速开始

**1. 先运行 `kingdom doctor`**

```bash
$ kingdom doctor

[系统]
✓ tmux 3.3a
✓ git 2.42
✓ claude   已安装
✗ codex    未找到  → npm install -g @openai/codex

[API 密钥]
✓ ANTHROPIC_API_KEY    已设置
✗ OPENAI_API_KEY       未设置  → export OPENAI_API_KEY=sk-...

[Kingdom 守护进程]
✗ 守护进程未运行
```

修复被标出来的问题，然后继续。

**2. 启动 Kingdom**

```bash
cd your-project
kingdom up
```

Kingdom 会：
- 创建一个名为 `kingdom` 的 tmux session
- 启动 daemon 进程（带 watchdog）
- 打开一个 2×2 pane 布局：manager + 最多 3 个 worker
- 在你的项目根目录下写入 `.kingdom/` 状态目录

如果之前存在一个带未完成 job 的 session，你会看到恢复提示：

```
检测到上次会话中未完成的工作：

  job_001  Implement auth middleware    [running → paused]
  job_002  Write frontend login call   [waiting]

Resume? [Y/n]
```

**3. 连接到现有会话**

```bash
kingdom attach          # 连接到默认的 "kingdom" 会话
kingdom attach myname   # 连接到一个命名会话
```

**4. 停止**

```bash
kingdom down            # 优雅停止 - 如果有任务在运行会提示
kingdom down --force    # 立即终止，保存 git diff
```

---

## CLI 参考

### 会话生命周期

| 命令 | 说明 |
|---|---|
| `kingdom up [workspace]` | 在指定目录中启动 Kingdom（默认：`.`） |
| `kingdom down [workspace]` | 优雅停止；如果有任务在运行会提示 |
| `kingdom down --force` | 立即终止，保存 git diff，并将任务标记为 `paused` |
| `kingdom attach [session]` | 将 tmux 附加到正在运行的会话（默认：`kingdom`） |
| `kingdom restart [workspace]` | 在不中断提供方的情况下重启守护进程 |

### Workers

```bash
kingdom swap <workspace> <worker-id>             # 让 Kingdom 选择替换的 provider
kingdom swap <workspace> <worker-id> <provider>  # 切换到指定 provider
```

### Observability

```bash
kingdom log [workspace]                  # 列出所有任务
kingdom log [workspace] --detail <id>    # 查看单个任务的完整时间线
kingdom log [workspace] --actions        # 原始动作流（action.jsonl）
kingdom log [workspace] --limit <n>      # 只显示最近 N 个任务

kingdom cost [workspace]                 # 按 provider 统计今天 / 本周 / 本月花费

kingdom doctor [workspace]               # 带修复提示的环境检查
```

### 维护

```bash
kingdom clean [workspace]           # 删除已完成任务的分支
kingdom clean [workspace] --all     # 也删除 paused/failed 分支
kingdom clean [workspace] --dry-run # 预览将要删除的内容
```

### 调试

```bash
kingdom replay <workspace> <job-id>    # 重放某个任务的动作流以便检查
kingdom job-diff <workspace> <job-id>  # 显示某个任务生成的 git diff
kingdom open <workspace> <target>      # 在 $EDITOR 中打开一个任务分支
```

---

## 配置

Kingdom 会在你的项目根目录中查找 `.kingdom/config.toml`。所有字段都是可选的，下面显示的是默认值。

```toml
[tmux]
session_name = "kingdom"       # tmux 会话名

[idle]
timeout_minutes = 30           # 多久没有活动后将 worker 标记为空闲

[health]
heartbeat_interval_seconds = 30   # 期望 worker 上报心跳的频率
heartbeat_timeout_count    = 2    # 丢失多少次心跳后标记为不健康
process_check_interval_seconds = 5
progress_timeout_minutes   = 30   # 在此时间内没有 job.progress → 触发 ProgressTimeout 事件

[failover]
window_minutes             = 10   # 故障率统计窗口
failure_threshold          = 3    # 窗口内达到多少次故障后触发故障切换
cooldown_seconds           = 30   # 允许下一次故障切换前的等待时间
connect_timeout_seconds    = 15   # 新 provider 必须在这段时间内连接成功
swap_checkpoint_timeout_seconds = 10
cancel_grace_seconds       = 30

[notifications]
mode = "poll"                  # "poll" | "push"（push 需要 webhook）

[webhook]
url     = ""                   # 事件的 POST 目标（留空表示禁用）
timeout_seconds = 5
events  = ["job.completed", "job.failed", "failover.triggered"]

[cost]
# 各 provider 的 token 单价（USD / 每 100 万 tokens）。
# provider 定价变化时请更新这里。
claude_input_per_1m  = 3.00
claude_output_per_1m = 15.00
codex_input_per_1m   = 2.50
codex_output_per_1m  = 10.00
gemini_input_per_1m  = 0.075
gemini_output_per_1m = 0.30
```

---

## 工作原理

### Components

```
kingdom up
  ├── daemon          Unix socket 服务器，持有 session 状态
  │     ├── MCP server    供 manager + workers 调用的工具
  │     ├── health monitor  每个 worker 的心跳 + 进程检查
  │     └── failover machine  检测故障 → 触发 handoff
  ├── watchdog        独立进程，在 daemon 崩溃时重启它
  ├── manager pane    tmux 中运行的一个 AI provider（读取工作区、分发 jobs）
  └── worker panes    每个 pane 一个 provider，都通过 MCP 连接
```

### MCP 协议

所有通信都通过 Unix socket 上的 MCP tool call 进行，没有 screen scraping，也没有 pane injection。worker 会调用 `job.progress`、`job.checkpoint`、`job.done` 之类的工具；manager 会调用 `worker.create`、`worker.send`、`workspace.status`。Kingdom 是唯一事实来源；provider 的自报状态在验证前都不可信。

### 故障切换

当某个 worker 失败时（心跳超时、进程退出、API 错误）：

1. Kingdom 通过 health monitor 检测到故障
2. 一个 tmux popup 会显示故障原因和压缩后的 job 状态简报
3. 用户确认，或者选择另一个 provider
4. Kingdom 在同一个 pane 中启动新的 provider
5. 新 provider 收到 briefing，并从最近一次 checkpoint 继续
6. 一行 `⚡ HANDOFF` 会写入 pane 的滚动历史，作为永久审计记录

job 会继续执行，不会丢失 context，也不需要手动重建。

### Disk State

```
.kingdom/
  daemon.pid          正在运行的 daemon PID
  kingdom.sock        Unix socket（路径会随 workspace hash 变化）
  config.toml         你的配置（可选）
  session.json        当前 session：jobs、workers、manager
  action.jsonl        append-only 事件日志（log/replay 的唯一事实来源）
  cost.json           各 provider 的 token 用量
```

---

## 贡献

```bash
cargo test          # 运行全部测试（215 个通过）
cargo clippy        # 提交前必须保持干净
cargo fmt           # 标准格式化
```

PR 应包含新行为对应的测试。`tests/` 中的集成测试会启动真实的 tmux 会话 - 在推送前请在本地运行它们，CI 会捕捉回归，但速度较慢。

Bug 报告：请附上 `kingdom doctor` 的输出和 `.kingdom/action.jsonl`（请遮盖任何秘密信息）。

## 许可证

MIT
