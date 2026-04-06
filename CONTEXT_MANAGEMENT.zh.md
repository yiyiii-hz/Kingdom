# Kingdom v2 设计：Context 管理

## 目标

在不超出 provider context 限制的前提下，让 provider 拥有完成当前任务所需的最小必要信息。

---

## Manager vs Worker 的 Context 策略差异

| | Worker | Manager |
|---|---|---|
| Context 内容 | 任务导向，做完可扔 | 关系导向，记住偏好和决策 |
| 超限处理 | Checkpoint 压缩 + 继续 | 直接 failover，不做摘要 |
| 新实例接手依据 | Checkpoint 摘要 + git diff | Kingdom 结构化状态（job / notes / log）|
| 理由 | 摘要足以继续任务 | 真正的"记忆"在 Kingdom 里，对话历史是过程 |

Manager 不实现 checkpoint 机制。Context 超限阈值触发时直接走 Manager failover 流程。

---

## 两层策略

### 层 1：Worker 自主 Checkpoint（防止超限）

Worker 自己做阶段性总结，不依赖外部 LLM，质量最高。

**触发时机（分级通知）：**

```
50%  →  urgency="normal"    最多延迟 60 秒
70%  →  urgency="high"      最多延迟 15 秒
85%  →  urgency="critical"  不允许延迟，必须立刻做
90%  →  触发 failover，用最近的 checkpoint 作为交接简报
```

**Checkpoint 流程：**

1. Kingdom 发送：`context.checkpoint_request(job_id, urgency)`
2. Worker 响应：
   - 立刻做：`job.checkpoint(job_id, summary)`
   - 申请延迟：`context.checkpoint_defer(job_id, reason, eta_seconds)`（仅 normal/high 允许）
3. Kingdom 存储 checkpoint + 自动附加 git diff
4. Worker 裁剪旧对话历史，context 回落到约 15%

**强制降级：** 超过延迟窗口仍未 checkpoint，Kingdom 用 git diff 生成降级版 checkpoint（无文字摘要）

**Checkpoint 强制模板（worker 必须回答以下五项 + Kingdom 自动附加 git 快照）：**

```
1. 做了什么      已完成的工作，具体到文件/函数
2. 放弃了什么    关键决定及原因（最重要，防止新 provider 重蹈覆辙）
3. 正在做什么    进行中的工作，精确到哪一步
4. 还剩什么      待完成清单
5. 踩过哪些坑    让新 provider 避开的已知问题

--- Kingdom 自动附加 ---
git_diff:         当前所有未提交改动的完整 diff
changed_files:    新增/修改/删除的文件列表
```

**新 provider 接手时的初始 context：**

```
以下文件已被上一个 provider 修改，不要重新实现：
  - src/auth/LoginForm.tsx（已完成验证逻辑）
  - src/auth/validation.ts（已完成，勿改）

当前 diff：[git diff 内容]

从这里继续：[checkpoint 第 3 项内容]
```

**质量保障（两层）：**

- **Kingdom 基础校验**：五项均不为空、每项不少于 20 字，不通过要求重填
- **Manager 审阅**：仅在 failover 触发时，popup 展示 checkpoint 内容供 manager 确认
  - 满意 → 确认切换
  - 不满意 → 要求 worker 补充（worker 还活着时）或直接用现有内容切换（worker 已崩溃时）

**Token 节省效果：**

| | 无 checkpoint | 有 checkpoint |
|---|---|---|
| 正常工作 | context 线性增长到崩溃 | 周期性回落，始终保持低位 |
| 失败重启 | 新 provider 从零开始 | 用 checkpoint 接手，省 60-80% |
| 总消耗 | 高（崩溃重来代价大）| 低（每次 checkpoint 约 500-1000 token，裁掉 2-3 万）|

### 层 2：结构化任务传递（减少初始 context）

Manager 向 worker 派发任务时，不传递完整对话历史。

只传递：
- 任务描述（用户 intent）
- 必要的代码/文件上下文（Kingdom 按需读取，不是全量传入）
- 相关的之前决定（来自 handoff summary，不是原始对话）

---

## 切换时的交接简报

当 provider 需要被替换时，Kingdom 生成交接简报传给新 provider：

```
[Kingdom Handoff Brief]
原 provider：Codex（原因：context 超限）
Job：{job intent}

已完成：
  - 实现了登录表单的 email/password 验证
  - 添加了 inline error 提示

进行中（写到一半）：
  - 表单提交处理（src/auth/submit.ts，第 45 行开始）

待完成：
  - 错误状态的 UI 样式
  - 单元测试

⚠ 可能不完整的文件（崩溃时正在写入，请先检查）：
  - src/auth/submit.ts

相关文件：
  - src/auth/LoginForm.tsx
  - src/auth/validation.ts
```

**崩溃时正在写入的文件：** Kingdom 通过比较崩溃前最后一次 `job.progress` 和 git diff 判断哪些文件在崩溃瞬间处于写入状态，在 handoff 简报里单独标注，不自动回滚，由新 provider 判断是继续还是重写。

新 provider 从这份简报开始，不需要也拿不到原始对话历史。

---

## Token 使用量追踪

### 追踪策略

**主要来源：Provider 强制上报**
- `context.ping` 是接入 Kingdom 的强制协议，不实现就不启动
- 每隔 30 秒或每次 tool call 响应后上报一次
- 各 provider 的数据来源：
  - Claude：`usage.input_tokens`
  - Codex：OpenAI usage 字段
  - Gemini：`usageMetadata.totalTokenCount`

**降级方案：Kingdom 自己估算（兜底）**
- 超时未收到 `context.ping` 时启用
- Kingdom 拦截 MCP 消息累加估算（不精准，但有总比没有好）
- 触发阈值从 70% 降到 50%（更保守）
- status bar 标记：`⚠ token tracking degraded`

### 各 Provider 上限

| Provider | Context 上限 | 触发压缩阈值（70%） |
|---|---|---|
| Claude | 200k tokens | 140k |
| Codex (GPT-4o) | 128k tokens | 90k |
| Gemini | 1M tokens | 700k |

Codex 风险最高，长任务优先监控。

### 设计原则

宁可压缩早了，不要等到 API 报错再处理。估算有 10-20% 误差没关系，70% 阈值留了足够余量。

---

## Token 节省预期

| 场景 | 无 Kingdom | 有 Kingdom |
|---|---|---|
| 长任务（2小时） | 撑爆，手动重建 | 自动压缩，继续运行 |
| 多 worker 并行 | 各自膨胀 | 各自独立压缩 |
| provider 切换 | 新 provider 从零开始 | 新 provider 拿压缩简报，节省 60-80% token |
