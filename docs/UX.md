# Kingdom v2 设计：UX

> Bilingual versions: [English](./UX.en.md) | [中文](./UX.zh.md)

## Tmux 布局

**主 window（kingdom:main）：** manager + 最多 3 个 worker，2x2 布局。

```
┌─────────────────────────────────────────────────────────┐
│  kingdom:main                                    [tmux]  │
├──────────────────────┬──────────────────────────────────┤
│  manager             │  worker-1                        │
│  [Claude]            │  [Codex]                         │
│                      │                                  │
│  正在规划任务...     │  正在实现登录验证...             │
│                      │                                  │
├──────────────────────┼──────────────────────────────────┤
│  worker-2            │  worker-3                        │
│  [Gemini]            │  [空闲]                          │
│                      │                                  │
│  正在写前端样式...   │  等待任务                        │
│                      │                                  │
├──────────────────────┴──────────────────────────────────┤
│ [Claude:mgr] [Codex:w1] [Gemini:w2] [idle:w3]  $0.34  12:34 │
└─────────────────────────────────────────────────────────┘
```

**超出 3 个 worker 时：** 新开 tmux window，每个 window 放一个 worker。

```
┌─────────────────────────────────────────────────────────┐
│  kingdom:worker-4                                [tmux]  │
├─────────────────────────────────────────────────────────┤
│  worker-4                                               │
│  [Codex]                                                │
│                                                         │
│  正在写单元测试...                                       │
│                                                         │
├─────────────────────────────────────────────────────────┤
│ [Claude:mgr] [Codex:w1] [Gemini:w2] [idle:w3] [Codex:w4]  12:34 │
└─────────────────────────────────────────────────────────┘
```

Status bar 跨 window 显示所有 worker，用户用标准 tmux 快捷键（`Ctrl+b n`）切换 window。

主 window 的 worker 上限可在 `config.toml` 调整（默认 3）：

```toml
[workers]
main_window_max = 3
```

---

## Status Bar（底部常驻）

格式：`[provider:role] [provider:role] ...  今日花费  时间`

示例：
```
[Claude:manager] [Codex:impl✓] [Gemini:ui⚠] [idle]  $0.34  14:32
```

图标含义：
- 无图标：正常运行
- `✓`：刚完成任务
- `⚠`：有 attention 需要处理
- `✗`：失败，等待处理
- `↻`：正在切换
- `⏳`：rate limited

## Token 花费可见性

`kingdom cost` 查看详细分解：

```
今日花费：$0.34
  Claude   128k tokens   $0.19  ████████░░
  Codex     89k tokens   $0.11  █████░░░░░
  Gemini    45k tokens   $0.04  ██░░░░░░░░

本周：$2.17  本月：$8.43

最贵的 job：job_003（实现登录验证）$0.18
```

各 provider 单价内置，可在 `.kingdom/config.toml` 更新。

---

## 离开时通知

默认不发通知。用户可在 `.kingdom/config.toml` 按事件类型配置：

```toml
[notifications]
on_job_complete = "none"         # none / bell / system
on_attention_required = "bell"   # 需要用户确认时（failover、blocking request）
on_job_failed = "system"
```

- `bell`：向终端发送 `\a`，tmux 可将 bell 转发为系统通知（用户自行配置 `set-option -g bell-action any`）
- `system`：macOS 用 `osascript`，Linux 用 `notify-send`，平台不支持时静默降级为 bell

---

## 三层信息架构

| 层 | 位置 | 内容 | 更新频率 |
|---|---|---|---|
| 常驻 | status bar | 每个 pane 的 provider + 状态 | 实时 |
| 事件 | popup | 切换详情、确认请求 | 事件触发 |
| 历史 | pane scroll | 工作内容 + HANDOFF 分隔线 | 永久记录 |

---

## Popup 设计

**切换确认 popup：**
- 显示失败原因
- 显示交接简报摘要（让用户确认 context 完整）
- 推荐的替换 provider
- 三个操作：确认切换 / 选择其他 provider / 暂停任务

**Popup 触发条件：**
- Provider 失败需要切换
- Context 即将超限（提前警告，而非等到失败）
- Job 完成（如果需要 manager review）

**Popup 不触发的情况：**
- 正常的 context 压缩（后台静默进行）
- status bar 更新
- pane 内的 progress 输出

---

## HANDOFF 分隔线

在 pane 的滚动历史中插入，作为永久审计记录：

```
────────────────────────────────────────────────────
⚡ HANDOFF  Codex → Claude                  14:32:01
原因: Context 超限 (98k tokens)
已传递: 登录验证前三步完成，表单提交处理进行中
────────────────────────────────────────────────────
```

---

## `kingdom log` 输出格式

**默认：Job 列表**

```
$ kingdom log

job_003  ✓  添加单元测试              completed  14:21  Codex    3m12s
job_002  ✓  写前端调用登录接口        completed  13:45  Gemini   8m04s
job_001  ✓  实现登录验证              completed  12:58  Codex    22m31s
            ↳ failover: Codex→Claude 14:02  context 超限
```

**`--detail <job_id>`：单个 job 完整时间线**

```
$ kingdom log --detail job_001

job_001  实现登录验证
  created   12:58  by manager
  worker    Codex（12:58 → 14:02）
  failover  14:02  context 超限 → Claude
  worker    Claude（14:02 → 13:20）
  completed 13:20  3 files changed
  branch    kingdom/job_001

  checkpoints:
    13:15  [kingdom checkpoint] 验证逻辑完成，表单提交进行中
    13:19  [kingdom checkpoint] 表单提交完成，待写测试
```

**`--actions`：原始操作流（action.jsonl 的可读版）**

```
$ kingdom log --actions

14:21  manager    job.complete   job_003
14:20  worker-2   job.progress   job_003  "测试全部通过"
14:02  kingdom    failover       job_001  Codex→Claude
13:15  worker-1   job.checkpoint job_001
```

---

## `kingdom doctor` 诊断输出

```
$ kingdom doctor

检查 Kingdom 运行环境...

[系统依赖]
✓ tmux 3.3a
✓ git 2.42
✗ codex    未安装  → npm install -g @openai/codex
✓ claude   已安装

[API Key]
✓ ANTHROPIC_API_KEY    已设置
✗ OPENAI_API_KEY       未设置  → export OPENAI_API_KEY=sk-...
✓ GEMINI_API_KEY       已设置

[Kingdom Daemon]
✓ daemon 运行中  PID 12345  已运行 2h34m
✓ MCP socket    /tmp/kingdom/a3f9c2.sock
✓ watchdog      运行中  PID 12346

[当前 Session]
✓ manager    Claude   已连接  context 23%
⚠ worker-1  Codex    心跳超时 45s  → kingdom swap worker-1
✓ worker-2  Gemini   已连接  context 41%

[配置文件]
✓ .kingdom/config.toml    有效
✓ KINGDOM.md              存在
⚠ .kingdom/manager.json  MCP socket 路径过时  → kingdom up --refresh-config
```

每个问题都给出具体修复命令，不只是报错。Kingdom 未运行时只检查系统依赖和配置文件层。

---

## 关停 UX

**无运行中 job：**
```
$ kingdom down
✓ Kingdom 已停止
```

**有运行中 job：**
```
$ kingdom down

有 2 个运行中的 job：
  job_001  实现登录验证   [Codex 运行中]
  job_002  写前端调用     [Gemini 运行中]

[等待完成]  [暂停并退出]  [强制退出]
```

- **等待完成**：等所有 job 完成或失败后自动停止
- **暂停并退出**：给每个 worker 发 checkpoint 请求（10 秒），然后停止所有进程，job 标记 `paused`
- **强制退出**：立刻 kill 所有进程，尽力保存 git diff，job 标记 `paused`

`kingdom down --force` 直接走强制退出路径，不询问。

---

## Session 恢复 UX

`kingdom up` 检测到未完成工作时，显示恢复摘要：

```
$ kingdom up

检测到上次 session 有未完成工作：

  job_001  实现登录验证          [running → Codex 已暂停]
  job_002  写前端调用登录接口     [waiting → 依赖 job_001]
  job_003  添加单元测试           [completed ✓]

workspace.notes:
  · 用 TypeScript，禁止 any
  · src/auth/ 不引入新依赖

继续上次工作？[Y/n]
```

确认后：
1. 重建 tmux session
2. 重启 manager provider
3. 恢复 worker（未完成的 job 带 checkpoint 接续）
4. Manager 收到恢复摘要，用户直接说"继续"即可

---

## 用户交互入口

| 操作 | 方式 |
|---|---|
| 查看当前状态 | status bar 常驻可见 |
| 切换 provider | popup 确认 |
| 手动触发切换 | `kingdom swap <worker>` 或 `kingdom swap <worker> <provider>` |
| 查看 job 历史 | `kingdom log` |
| 诊断问题 | `kingdom doctor` |
| 启动 | `kingdom up` |
| 停止 | `kingdom down` / `kingdom down --force` |
| 重启 daemon | `kingdom restart`（session 和 provider 不中断）|
| 回到已有 session | `kingdom attach` |
