# Kingdom v2 设计：Failover

## 用户手动干预

用户直接 `Ctrl+C` 或 `tmux kill-pane` 时，Kingdom 通过 exit code 判断：

```
exit 0              → 用户主动退出，job 标记 paused，不触发 failover
exit != 0           → provider 崩溃，触发 failover 流程
tmux kill-pane      → 进程消失，按崩溃处理
```

**5 秒缓冲窗口：**

进程退出后 Kingdom 等待 5 秒，弹出 popup：

```
┌─────────────────────────────────┐
│  Codex 进程已退出               │
│                                 │
│  是你手动停止的吗？             │
│                                 │
│  [是，暂停任务]  [否，触发切换] │
└─────────────────────────────────┘
```

- 用户确认"是"→ job 标记 `paused`，不触发 failover
- 用户确认"否"或 5 秒无响应 → 按崩溃处理，触发 failover

---

## 检测

Kingdom 持续监控每个 provider 进程（通过 MCP 心跳 + 进程状态）。

触发条件：
- 网络中断：MCP 连接断开超过 N 秒
- Context 超限：API 返回 context length error
- 进程退出：provider 进程非正常退出
- 超时：超过配置时间无 job.progress / job.complete 响应

---

## 切换流程

```
1. Kingdom 检测到失败
       ↓
2. Kingdom 暂停该 pane 的输出
       ↓
3. tmux display-popup 弹出确认框
   ┌─────────────────────────────────┐
   │ ⚠️  Codex 失败                  │
   │ 原因：Context 超限 (98k tokens) │
   │                                 │
   │ 交接简报已准备好：              │
   │ · 已完成：登录验证前三步        │
   │ · 进行中：表单提交处理          │
   │ · 待完成：错误提示 UI           │
   │                                 │
   │ 切换至：Claude（推荐）          │
   │                                 │
   │ [确认切换]  [选择其他]  [取消]  │
   └─────────────────────────────────┘
       ↓ 用户确认
4. Kingdom 在同一个 pane 启动新 provider
       ↓
5. pane 输出一行分隔线：
   ────────────────────────────────────────
   ⚡ HANDOFF  Codex → Claude
   原因: Context 超限 (98k tokens)
   已传递: 登录验证进行中，表单提交待完成
   ────────────────────────────────────────
       ↓
6. 新 provider 收到交接简报，继续工作
       ↓
7. status bar 更新显示新 provider
```

---

## Manager 失败

和 worker 失败相同流程。

特殊点：manager 失败时，所有 worker 会暂停接收新任务，等待新 manager 接手后继续。

---

## Rate Limit 处理

Rate limit（429）不触发 failover，单独处理：

```
检测到 rate_limit
      ↓
指数退避重试：5s → 15s → 30s → 60s
status bar 显示：[Codex:impl ⏳ rate limited]
      ↓
重试成功 → 恢复正常工作
重试 3 次仍失败 → 降级为崩溃处理，触发 failover
```

---

## 熔断机制

防止级联失败无限循环。

**触发条件：** 10 分钟内同一 job 失败 ≥ 3 次

**触发后：**
- Job 标记为 `paused`
- 不再自动触发 failover
- Popup 通知用户：`job_001 连续失败 3 次，请检查任务或手动选择下一步`
- Status bar 标记：`[job_001 ⛔]`

**冷却时间：** 两次 failover 之间至少间隔 30 秒，防止新 provider 未稳定就被误判失败

**可配置项（.kingdom/config.toml）：**
```toml
[failover]
window_minutes = 10      # 时间窗口
failure_threshold = 3    # 窗口内失败次数上限
cooldown_seconds = 30    # 两次 failover 最小间隔
```

---

## Provider 启动失败

启动失败和运行时崩溃区别处理，启动失败不触发 failover：

| 情况 | 表现 | 处理 |
|---|---|---|
| binary 找不到 | 启动时报错 | 告知用户安装，job paused |
| 启动后立刻崩溃 | 进程存活 < 3 秒 | 告知用户检查配置，job paused |
| 启动了但不连 MCP | 15 秒超时 | 杀掉进程，告知用户，job paused |
| 运行时崩溃 | 进程退出（exit != 0）| 触发 failover |

启动失败统一给出可操作的错误提示：
```
binary 找不到  → "请先安装 codex：npm install -g @openai/codex"
启动即崩溃    → "codex 启动失败（exit 1），请检查配置"
MCP 连接超时  → "codex 启动成功但未连接 Kingdom，请检查 MCP 配置"
```

---

## Failover 空窗期 UX

旧 provider 崩溃到新 provider 连接成功期间，pane 显示过渡状态：

```
────────────────────────────────────────────────
⚡ HANDOFF  Codex → Claude                14:32:01
原因: Context 超限 (98k tokens)
────────────────────────────────────────────────
⏳ 正在启动 Claude... (3s)
```

- 每秒更新计时，让用户知道系统在处理
- 启动成功后过渡行消失，新 provider 输出开始出现
- 超过 15 秒未连接，显示警告并提供操作选项：
  ```
  ⚠ Claude 启动超时
  [重试]  [换其他 provider]  [暂停任务]
  ```

---

## 推荐的替换 provider

Kingdom 按以下顺序推荐：

1. 同类型中其他可用的 provider（比如 Codex 失败，推荐另一个 Codex 实例）
2. 能力最接近的 provider（按内置能力偏好表）
3. 用户手动选择

用户可以覆盖推荐，选择任意可用 provider。

---

## Manager Failover

Manager 失败时，新 manager 从 Kingdom 的结构化数据恢复，不依赖对话历史。

**新 manager 启动时收到的完整包：**

```
1. KINGDOM.md          工程约束（技术选型、代码规范、架构决定）
2. CLAUDE.md           行为约束（输出格式、语言风格）
3. workspace.notes     会话约束（manager 通过 workspace.note() 捕获）
4. 所有 job 状态       当前进度、checkpoint、分配的 worker
5. 待处理队列          paused jobs、blocking requests、待确认问题
6. 最近 N 条 action log 了解刚刚发生了什么
```

**`KINGDOM.md` 格式：** 纯自由 Markdown，Kingdom 整段传给 provider 作为 system prompt 的一部分，不做结构化解析。机器行为配置放 `.kingdom/config.toml`，AI 行为约束放 `KINGDOM.md`，职责分离。

**优先级：** KINGDOM.md（工程约束）> CLAUDE.md（行为约束）

冲突时以 KINGDOM.md 为准，`kingdom doctor` 扫描并提示用户合并重复约束。

**隐性上下文保留机制：**

- `KINGDOM.md`：用户把长期有效的约束写在这里，新 manager 自动读入
- `workspace.note(constraint)`：manager 在对话中捕获用户的隐性约束，Kingdom 持久化

```
用户："这个功能要兼容 Safari 14"
Manager 调用：workspace.note("兼容 Safari 14，不要用新 CSS 特性")
```

**Manager 工具集补充：**

```
workspace.note(constraint, scope?)   // 记录约束，scope 可选：global / 目录路径 / job_id
workspace.notes()                    // 查看所有已记录约束
```

**Note 优先级：** scope 越窄优先级越高（job > 目录 > global）

**冲突检测：** Kingdom 不做自动冲突检测。Manager 每次接手时读取全部 note（`workspace.notes()`），自行判断是否有冲突并调用 `workspace.note()` 清理。

---

## Manager 断连期间

**Kingdom 的职责：**
- `job.complete()` 立刻写入 `.kingdom/`，不等 manager
- 所有 worker 事件排队缓存
- Manager 恢复后主动推送摘要：完成的 job、暂停的 job、待确认的问题

**Worker 的行为：**
- 遇到 `blocking=true` 的 `job.request()`：暂停等待，不继续执行
- 遇到 `blocking=false` 的 `job.request()`：继续工作，问题进入待确认队列
- 正常完成：调用 `job.complete()`，Kingdom 存储，等 manager 恢复后审阅

---

## 取消切换

用户选择取消时：
- job 标记为 `paused`
- pane 显示 `[Kingdom] 任务已暂停，等待手动处理`
- 用户可以之后手动触发切换

---

## 手动 Swap（`kingdom swap`）

用户主动发起切换，走同一套 failover 切换流程。

**命令形式：**

```
kingdom swap worker-1              # 弹出可用 provider 列表供选择
kingdom swap worker-1 claude       # 直接指定目标 provider
```

**流程：**

```
1. Kingdom 向当前 worker 发送 checkpoint 请求（urgency="high"）
2. 等待最多 10 秒
   → worker 在 10 秒内提交 checkpoint → 用 checkpoint 做 handoff
   → 超时 → 用 git diff 生成降级 checkpoint，强制切换
3. 弹出确认框（同 failover popup，原因改为"用户手动切换"）
4. 用户确认 → 走标准 failover 切换流程
```

与自动 failover 的唯一区别：原因标记为 `manual`，不计入熔断计数。
