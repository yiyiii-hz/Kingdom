# Kingdom v2

原生于终端的 AI 工作者编排。多个提供方，一个会话，自动故障切换。

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

当提供方出错时，Kingdom 会继续保持工作运行。当 Codex 遇到上下文限制或断开时，Kingdom 会检测到这一点，先请求确认，然后在同一窗格内带着压缩后的简报把任务交接给 Claude。整个流程无需手动重建，工作可持续推进。

---

## 需求

- **Rust** 1.75+
- **tmux** 3.0+
- **git** 2.30+
- 至少安装并完成认证的一个 AI 提供方 CLI：
  - [claude](https://github.com/anthropics/anthropic-quickstarts) (`ANTHROPIC_API_KEY`)
  - [codex](https://github.com/openai/codex) (`OPENAI_API_KEY`)
  - [gemini](https://ai.google.dev/) (`GEMINI_API_KEY`)

## 安装

```bash
git clone https://github.com/your-org/kingdom-v2
cd kingdom-v2
cargo build --release
cp target/release/kingdom ~/.local/bin/
```

验证：

```bash
kingdom --help
kingdom doctor
```

`kingdom doctor` 会检查你的环境，并明确告诉你还缺什么。

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

把标出的项修好，然后继续。

**2. 启动 Kingdom**

```bash
cd your-project
kingdom up
```

Kingdom 会：
- 创建一个名为 `kingdom` 的 tmux 会话
- 启动守护进程（带看门狗）
- 打开一个 2×2 的窗格布局：管理者 + 最多 3 个工人
- 在你的项目根目录下写入 `.kingdom/` 状态目录

如果之前的会话里还有未完成的任务，你会看到恢复提示：

```
检测到上次会话中未完成的工作：

  job_001  Implement auth middleware    [running → paused]
  job_002  Write frontend login call   [waiting]

恢复？ [Y/n]
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

### 工人

```bash
kingdom swap <workspace> <worker-id>             # 让 Kingdom 选择替换的提供方
kingdom swap <workspace> <worker-id> <provider>  # 切换到指定提供方
```

### 可观测性

```bash
kingdom log [workspace]                  # 列出所有任务
kingdom log [workspace] --detail <id>    # 查看单个任务的完整时间线
kingdom log [workspace] --actions        # 原始动作流（action.jsonl）
kingdom log [workspace] --limit <n>      # 只显示最近 N 个任务

kingdom cost [workspace]                 # 按提供方统计今天 / 本周 / 本月支出

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

Kingdom 会在你的项目根目录中查找 `.kingdom/config.toml`。所有字段都是可选的，下面展示的是默认值。

```toml
[tmux]
session_name = "kingdom"       # tmux 会话名

[idle]
timeout_minutes = 30           # 在这么长时间没有活动后，将工人标记为空闲

[health]
heartbeat_interval_seconds = 30   # 期望从工人收到心跳的间隔
heartbeat_timeout_count    = 2    # 丢失多少次心跳后标记为不健康
process_check_interval_seconds = 5
progress_timeout_minutes   = 30   # 在此时间内没有 job.progress → 触发 ProgressTimeout 事件

[failover]
window_minutes             = 10   # 故障率统计窗口
failure_threshold          = 3    # 窗口内达到多少次故障后触发故障切换
cooldown_seconds           = 30   # 允许下一次故障切换前的等待时间
connect_timeout_seconds    = 15   # 新提供方必须在此时间内连上
swap_checkpoint_timeout_seconds = 10
cancel_grace_seconds       = 30

[notifications]
mode = "poll"                  # "poll" | "push"（push 需要 webhook）

[webhook]
url     = ""                   # 事件的 POST 目标（留空表示禁用）
timeout_seconds = 5
events  = ["job.completed", "job.failed", "failover.triggered"]

[cost]
# 每个提供方的 token 价格（USD / 100 万 tokens）。
# 当提供方定价变化时请更新这里。
claude_input_per_1m  = 3.00
claude_output_per_1m = 15.00
codex_input_per_1m   = 2.50
codex_output_per_1m  = 10.00
gemini_input_per_1m  = 0.075
gemini_output_per_1m = 0.30
```

---

## 工作原理

### 组件

```
kingdom up
  ├── daemon          Unix socket 服务器，拥有会话状态
  │     ├── MCP server    供管理者 + 工人调用的工具
  │     ├── health monitor  每个工人的心跳 + 进程检查
  │     └── failover machine  检测故障 → 触发交接
  ├── watchdog        独立进程，在 daemon 崩溃时重启它
  ├── manager pane    在 tmux 中运行的一个 AI 提供方（读取工作区，分发任务）
  └── worker panes    每个窗格一个提供方，通过 MCP 连接
```

### MCP 协议

所有通信都通过 Unix socket 上的 MCP 工具调用进行，没有屏幕抓取，也没有窗格注入。工人会调用 `job.progress`、`job.checkpoint` 和 `job.done` 之类的工具；管理者会调用 `worker.create`、`worker.send` 和 `workspace.status`。Kingdom 是唯一事实来源；在验证前，不会信任提供方的自报状态。

### 故障切换

当某个工人失败时（心跳超时、进程退出、API 错误）：

1. Kingdom 通过健康监控检测到故障
2. tmux 弹窗出现，显示故障原因和压缩后的任务状态简报
3. 用户确认（或选择其他提供方）
4. Kingdom 在同一窗格中启动新的提供方
5. 新提供方接收简报，并从上一个检查点继续
6. 一行 `⚡ HANDOFF` 会写入窗格滚动历史，作为永久审计记录

任务会继续执行，不会丢失上下文，也不需要手动重建。

### 磁盘状态

```
.kingdom/
  daemon.pid          正在运行的 daemon PID
  kingdom.sock        Unix socket（路径因工作区哈希而异）
  config.toml         你的配置（可选）
  session.json        当前会话：任务、工人、管理者
  action.jsonl        仅追加的事件日志（log/replay 的唯一事实来源）
  cost.json           每个提供方的 token 用量
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