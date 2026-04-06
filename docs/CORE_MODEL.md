# Kingdom v2 设计：核心模型

> Bilingual versions: [English](./CORE_MODEL.en.md) | [中文](./CORE_MODEL.zh.md)

## 三个核心概念

### 1. Job（任务）

用户意图的载体。一个 Job 有：

- `intent`：用户的原始描述
- `status`：pending / running / done / failed
- `history`：所有尝试的执行记录（谁做的、做了什么、结果如何）
- `handoff_summary`：最新的交接简报（切换时传给新 provider 的内容）

Job 是稳定的。provider 换了，job 不变。

**Job 生命周期：**

- `waiting`：有未完成的依赖，等待中
- `cancelling`：取消中（等待 worker graceful stop）

**Worker 生命周期：**

```
running → completed → idle → 超时自动终止
```

idle 超时默认 10 分钟，可在 `.kingdom/config.toml` 配置。超时后 Kingdom 终止 worker 进程。

Manager 需要新 worker 时始终走 `worker.create()`，获得新进程、新 context，不复用 idle worker。理由：带旧 context 复用风险高（context 污染 + 提前超限），而冷启动代价极低（秒级）。
- `pending`：依赖已满足，等待 manager 分配
- `running`：worker 正在执行
- `completed`：已完成
- `failed`：失败
- `paused`：用户或熔断机制暂停

**Job 依赖：**

```
job.create("写前端调用", depends_on=["job_A"])
```

- 依赖未完成时 job 状态为 `waiting`
- 依赖完成后 Kingdom 通知 manager，job 变为 `pending`，manager 决定是否启动
- 依赖失败时 Kingdom 通知 manager，job 保持 `waiting`，manager 决定后续：
  - `job.cancel(job_id)` — 取消
  - `job.keep_waiting(job_id)` — 等依赖修复后继续
  - `job.update(job_id, new_intent)` — 修改描述后立刻启动

**取消的级联处理：**

取消 job_A 时，Kingdom 检查是否有 job 依赖它，若有则通知 manager：

```
[Kingdom] 取消 job_001 会影响以下 job：
  job_002  写前端调用（waiting，依赖 job_001）
  job_003  添加单元测试（waiting，依赖 job_001）

请选择：[一并取消]  [保持等待]  [逐个决定]
```

默认不自动级联，由 manager 决定。与依赖失败的处理模式一致。

### 2. Provider

执行工作的 AI。默认三个：

| Provider | 默认擅长 | 默认模型 |
|---|---|---|
| Claude | 规划、推理、协调 | claude-sonnet-4-5 |
| Codex | 编码实现、重构、测试 | gpt-4o |
| Gemini | 前端、UI、文案 | gemini-2.0-flash |

**按 job 指定模型：**

```
job.create("设计整个认证架构", model="claude-opus")    // 复杂任务用强模型
job.create("写一个简单按钮", model="gemini-flash")     // 简单任务用便宜模型
```

Kingdom 内置成本感知提示：任务复杂度低时建议切换更便宜的模型。

可在 `.kingdom/config.toml` 全局覆盖默认模型。

这些是**默认推荐**，不是硬绑定。任何 provider 都可以担任任何角色。

### 3. Session

当前 workspace 的运行上下文。包含：

- 当前 manager 是谁
- 当前有哪些 worker（每个 pane 一个）
- 每个 worker 正在处理哪个 job
- action history（可审计）

---

## Git 策略

Kingdom 不强制 git 行为，提供可配置的默认策略。

**默认：每个 job 独立 branch**

```
job 启动 → git checkout -b kingdom/job_001
worker 在独立 branch 上工作
job 完成 → Kingdom 通知 manager，branch 待 review
manager 决定：merge / 继续改 / 丢弃
```

并行 worker 各自在独立 branch 上，互不干扰。Merge 冲突在 manager review 时处理。

**Failover 时：** 新 provider 在同一个 branch 继续，不开新 branch，历史保持连续。

**Commit 时机：**
- `job.checkpoint` → Kingdom 自动 commit，提交信息：`[kingdom checkpoint] job_001: {checkpoint 摘要}`
  - 每个 checkpoint 有干净的独立 diff，不累积
- `job.complete` → 不自动 commit，manager 决定：merge / squash merge / 丢弃 branch

**`strategy = "commit"` 时：** checkpoint 和 complete 都自动 commit 到当前 branch，无独立 branch。
**`strategy = "none"` 时：** Kingdom 完全不碰 git，checkpoint 只保存文字摘要，无 diff 快照。

**可配置策略（`.kingdom/config.toml`）：**

```toml
[git]
strategy = "branch"      # branch（默认）/ commit / none
branch_prefix = "kingdom/"
auto_commit = false       # job 完成时是否自动 commit
```

- `branch`：每个 job 独立 branch（推荐，并行安全）
- `commit`：在当前 branch 工作，job 完成后自动 commit
- `none`：Kingdom 不碰 git

---

## Role 的处理

v2 不把 role 暴露给用户。

用户只看到：**Job** 和 **Provider**。

role（manager / worker）是 Kingdom 内部概念，决定 provider 拿到哪套 MCP 工具权限。

---

## 完成信号

worker 通过 MCP tool call 告知 Kingdom 任务完成：

```
job.complete(job_id, result_summary)
job.fail(job_id, failure_reason)
job.progress(job_id, progress_note)  // 可选，用于长任务
```

Kingdom 不依赖文件系统 artifact（不再需要 done.json）。

---

## 失败分类

| 类型 | 描述 | 自动检测 |
|---|---|---|
| `network` | 网络中断 | ✓ |
| `context_limit` | token 超限 / API error | ✓ |
| `process_exit` | provider 进程退出 | ✓ |
| `timeout` | 超过预设时间无响应 | ✓ |
| `rate_limit` | API 调用频率超限（429）| ✓ |
| `manual` | 用户主动触发替换 | — |
