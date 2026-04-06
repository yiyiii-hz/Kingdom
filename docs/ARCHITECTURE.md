# Kingdom v2 设计：架构

> Bilingual versions: [English](./ARCHITECTURE.en.md) | [中文](./ARCHITECTURE.zh.md)

## 核心架构

```
┌─────────────────────────────────────────────────┐
│  Kingdom Core                                   │
│                                                 │
│  ┌──────────────┐    ┌──────────────────────┐   │
│  │  MCP Server  │    │  Process Manager     │   │
│  │              │    │                      │   │
│  │  工具权限仲裁 │    │  PID 追踪            │   │
│  │  Action 记录 │    │  健康监控            │   │
│  │  Context 追踪│    │  Failover 触发       │   │
│  └──────┬───────┘    └──────────┬───────────┘   │
│         │                       │               │
└─────────┼───────────────────────┼───────────────┘
          │ MCP (socket)          │ PID monitor
          │                       │
    ┌─────▼───────────────────────▼──────┐
    │         Provider Process           │
    │                                    │
    │  ┌─────────────┐  ┌─────────────┐  │
    │  │ MCP Client  │  │     tty     │  │
    │  │ (工具调用)  │  │  (显示+交互) │  │
    │  └─────────────┘  └──────┬──────┘  │
    └──────────────────────────┼─────────┘
                               │
                         tmux pane
                     (用户可见 + 可交互)
```

---

## 安全模型

**Prompt Injection 防护：两层机制**

不试图识别文件内容中的恶意指令（太难），而是限制 worker 能做什么：

**第一层：最小权限**
- Worker 默认只有最小工具集，没有 `shell.exec`、`workspace.delete` 等危险工具
- 即使被 inject，能造成的破坏范围极有限

**第二层：Kingdom 拦截异常工具调用**
- 所有工具调用经过 Kingdom，Kingdom 检测是否超出授权范围
- 异常时立刻拦截并告警：

```
⚠ 异常操作被拦截
  Worker job_001 尝试调用未授权工具：shell.exec("rm -rf .kingdom/")
  可能原因：读取的文件包含恶意指令
  [忽略]  [暂停 worker]  [终止 worker]
```

---

## 存储管理

**分层保留策略：**

| 数据类型 | 保留策略 |
|---|---|
| 未完成 job 的 checkpoint | 永远保留 |
| workspace.notes | 永远保留 |
| 最近 30 天 action log | 永远保留 |
| 已完成 job 最终结果 | 保留 90 天后归档 |
| 已完成 job 中间 checkpoint | 7 天后只保留最后一个 |
| 30 天前的详细 action log | 压缩为摘要 |

可在 `.kingdom/config.toml` 调整保留期限。

`kingdom clean` 手动触发清理，清理前显示将删除内容和释放空间，用户确认后执行：

```
$ kingdom clean

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

`kingdom clean --dry-run`：只显示，不执行。
`kingdom clean --all`：不按时间限制，清理所有可清理内容（慎用）。

---

## 数据一致性

**所有写操作经过 Kingdom，worker 不直接写文件系统。**

```
Worker → job.checkpoint() → MCP → Kingdom 队列 → 串行写盘
Worker → job.complete()   → MCP → Kingdom 队列 → 串行写盘
Worker → job.progress()   → MCP → Kingdom 队列 → 串行写盘
```

Kingdom 单进程处理所有 MCP 请求，写操作天然串行，无竞态条件。

**目录结构（每个 job 独立隔离）：**

```
.kingdom/
  state.json              全局状态（Kingdom 独占写）
  jobs/
    {job_id}/
      meta.json           job 元数据
      checkpoints/        checkpoint 历史
      handoff.md          最新交接简报
      result.md           最终结果
  logs/
    action.jsonl          全量操作日志（append-only）
```

---

## 两个独立通道

### MCP Channel（Kingdom 专用）
- Provider 连接到 Kingdom 的 MCP server
- 所有结构化通信走这里：job 汇报、权限申请、状态更新
- Kingdom 通过这里获取 authoritative 信息，不读 stdout

### TTY Channel（人类专用）
- Provider 进程的 tty 绑定到 tmux pane
- 用户可以直接看到 AI 在干什么
- 需要人工介入时（sudo 密码、紧急确认）用户直接在 pane 里打字
- pane 是**显示窗口 + 交互逃生口**，不是 Kingdom 的信息来源

**Worker pane 交互边界：**

Worker 启动时 pane 顶部显示一行静态提示，之后不重复：

```
[Kingdom] 直接在此输入不会被记录到 action history，仅用于紧急干预
```

用户直接在 worker pane 输入的内容 Kingdom 不感知，failover 时该部分上下文丢失。这是有意设计——pane 是逃生口，不是正式协作通道。

---

## Provider 启动流程

```
manager 调用 worker.create("codex")
       ↓
Kingdom Process Manager
  1. tmux split-window 创建新 pane
  2. 在 pane 里启动：codex --mcp-config worker.json
     （进程 tty 绑定到 pane，保持可交互）
  3. 记录 PID，开始监控
       ↓
Codex 进程启动
  1. MCP client 连接 Kingdom MCP server（独立 socket）
  2. 自动发现 worker 工具集
  3. 接收初始 context（job 描述 + 工作区信息）
  4. 开始工作，输出显示在 pane 里
       ↓
Kingdom
  1. 通过 MCP 心跳确认连接
  2. 开始追踪 token 使用量
  3. 更新 status bar
```

---

## 健康监控

Kingdom 通过两个维度监控每个 provider：

| 维度 | 方式 | 失败判定 |
|---|---|---|
| 进程存活 | PID 监控 | 进程退出（非正常 exit code），即时触发 |
| MCP 连通 | 心跳 ping（每 30 秒） | 连续 2 次未响应（60 秒）才触发 |
| Context 健康 | token 追踪 | 超过阈值（70%）|
| 任务响应 | job.progress 间隔 | 默认 30 分钟无上报→警告（不自动 failover）；弹出询问用户 |

进程 exit 和心跳超时分开处理：进程 exit 是确定崩溃，即时触发；心跳超时是卡死，需要更长确认窗口避免误触发。

**可配置项（`.kingdom/config.toml`）：**
```toml
[health]
heartbeat_interval_seconds = 30
heartbeat_timeout_count = 2         # 连续几次未响应才触发 failover
progress_timeout_minutes = 30       # 无 job.progress 上报多久后发警告（不自动 failover）
```

任何一个维度触发 → 进入 failover 流程。

---

## MCP Socket 管理

Kingdom MCP server 使用 Unix domain socket，按 workspace 路径生成唯一地址：

```
/tmp/kingdom/{workspace_hash}.sock
```

- `workspace_hash` = repo 根路径的 hash（如 `a3f9c2`）
- 每个 workspace 独立 socket，多个 workspace 同时运行互不干扰
- 无端口冲突，无防火墙问题

`kingdom up` 自动生成 MCP config，写入 socket 路径：

```json
{
  "mcpServers": {
    "kingdom": {
      "transport": "unix",
      "socket": "/tmp/kingdom/a3f9c2.sock"
    }
  }
}
```

Kingdom 重启后 socket 文件重新创建，provider 自动重连。

---

## Kingdom 自身可靠性

**每个 workspace 独立运行一个 Kingdom daemon，互相隔离。**

Kingdom daemon 以 daemon 形式运行，崩溃后自动重启：

```
kingdom up
  ↓
启动 watchdog 进程（轻量，只负责监控和重启本 workspace 的 Kingdom daemon）
  ↓
watchdog 启动 Kingdom daemon
  ↓
Kingdom daemon 崩溃时，watchdog 立刻重启它
```

多个 workspace 同时运行时，各自有独立的 daemon + watchdog + socket，互不影响。

**状态持久化原则：** 每次操作后立刻写盘，不依赖内存状态。Kingdom 重启后从 `.kingdom/` 完整恢复。

**重启后恢复流程：**
1. 读取 `.kingdom/` 恢复 workspace、job、worker 状态
2. 向所有还活着的 provider 重新建立 MCP 连接
3. 恢复 token 追踪计数
4. Status bar 恢复显示

**Provider 侧重连：** MCP 连接断开后指数退避重试（1s → 2s → 4s → … → 30s 封顶），无限重试直到 Kingdom 恢复。重连期间 tool call 本地缓存，Kingdom 恢复后补报。Provider 进程退出由 PID 监控处理，不走重连路径。

**Kingdom 重启后主动重连：** Kingdom 读取 `.kingdom/` 恢复状态后，向所有已知 provider 主动重建 MCP 连接。15 秒内无响应则标记该 provider 离线，触发 failover 流程。

---

## Kingdom 启动顺序

**已有 session 时的行为：**

| 情况 | 处理 |
|---|---|
| daemon + session 都在 | 提示 `kingdom attach` 回到现有 session 或 `kingdom restart` 重启 |
| daemon 在，session 丢了 | 自动重建 tmux session，恢复所有 job 状态 |
| session 名冲突（非 Kingdom session）| 报错提示，引导用户在 config.toml 配置不同 session 名 |

```toml
[tmux]
session_name = "kingdom"    # 可自定义避免冲突
```

---

```
kingdom up
  ↓
1. 检查 tmux（没有就报错引导安装）
   检查 git（没有则警告，询问是否以无 git 模式继续；确认后自动降级 `strategy = "none"`）
2. 初始化 `.kingdom/` 目录（已存在则跳过），创建 `.kingdom/.gitignore`（内容 `*`）
3. 探测可用 provider（which claude / codex / gemini ...）
   → 结果存入 session state
   → manager provider 不可用：报错退出
   → worker provider 不可用：警告，继续启动
   检测 API key 环境变量，缺失时提示（不阻断启动）：
   ```
   ✗ OPENAI_API_KEY 未设置（Codex 不可用）
     → export OPENAI_API_KEY=sk-...
   ```
   Kingdom 不存储或注入 API key，provider 进程继承当前 shell 环境变量。
4. 生成 MCP 配置（manager.json + worker.json）
5. 启动 Kingdom MCP server（后台 daemon）
6. 询问默认 manager provider（只列出探测到的可用项）
7. 创建 tmux session
8. 在 pane-0 里启动 manager provider
9. 等待 manager MCP 连接成功
10. 输出：✓ 启动完成
```

---

## MCP Config 结构

`kingdom up` 生成两份 MCP config，`role` 字段决定 Kingdom 下发哪套工具集：

```json
// .kingdom/manager.json
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

// .kingdom/worker.json
{
  "mcpServers": {
    "kingdom": {
      "transport": "unix",
      "socket": "/tmp/kingdom/a3f9c2.sock",
      "role": "worker",
      "session_id": "sess_abc123"
    }
  }
}
```

Worker config 在 `worker.create()` 时动态追加 `job_id`，Kingdom 据此关联工具调用到具体 job。

## Manager 初始 Prompt

Provider 连接后，Kingdom 通过 MCP 注入标准 manager system prompt：

```
你是 Kingdom 的 manager。
当前 workspace：{path}
可用 worker provider：{available_providers}
当前 job 状态：{workspace.status 快照}

你的职责：分析用户意图、拆分任务、派发给 worker、审查结果。
通过 MCP 工具与 Kingdom 交互，不要直接操作文件系统。
```

用户在 `KINGDOM.md` 里追加的内容附加在标准 prompt 之后，优先级更高。

---

## Provider 发现

**探测方式：** `which <provider_binary>` 主动探测，`.kingdom/config.toml` 可覆盖路径。

**探测结果：**
- 存入 session state（`available_providers`）
- Failover 推荐时只推荐可用 provider
- `kingdom up` 输出探测摘要：

```
✓ claude   已安装 (/usr/local/bin/claude)
✓ codex    已安装 (/usr/local/bin/codex)
✗ gemini   未找到（可选）
```

**Provider 启动模板（内置，可覆盖）：**

Kingdom 内置三个 provider 的启动参数，`{mcp_config}` 为自动替换的 config 路径占位符：

```toml
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

用户可在 `.kingdom/config.toml` 覆盖任意字段，追加自定义参数：

```toml
[providers.codex]
args = ["--mcp-config", "{mcp_config}", "--model", "gpt-4o"]

[providers.gemini]
binary = "/opt/custom/gemini-cli"
```

v2 只内置 claude / codex / gemini 三个 provider，自定义 provider 需用户手动配置完整 `args`。
