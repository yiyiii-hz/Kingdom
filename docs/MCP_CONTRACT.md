# Kingdom v2 设计：MCP Contract

> Bilingual versions: [English](./MCP_CONTRACT.en.md) | [中文](./MCP_CONTRACT.zh.md)

## 设计原则

- Manager 和 worker 拿到不同的工具集
- 默认软限制：worker 启动时拿最小权限
- Manager 可以动态授权 worker 更多权限
- 所有工具调用记录在 action history，可审计

---

## Job 完成展示

完成时三层同步：

```
Worker pane     显示完成总结 + "等待 manager 审阅..."
Status bar      [Codex:impl✓] 标记待处理
Manager pane    Kingdom 推送 notification，包含结果摘要和 changed files
```

Manager 调用 `job.result(job_id)` 获取完整结果，决定下一步。
Worker pane 不自动关闭，用户可继续查看工作过程。

---

## Manager 工具集

Manager 拥有全局控制权：

```
# Workspace
workspace.status()                          // 全局状态快照
workspace.log()                             // action history

# Worker 生命周期
worker.create(provider, role?)              // 创建 worker
worker.assign(worker_id, job_id)            // 分配任务
worker.release(worker_id)                   // 释放 worker（只能对 idle worker 调用）
                                           // 立刻终止进程，无需 checkpoint
                                           // worker 有 job 在跑时返回错误，需先 job.cancel
worker.swap(worker_id, new_provider)        // 手动切换 provider

# 约束管理
workspace.note(constraint)                  // 记录隐性约束（manager 在对话中捕获）
workspace.notes()                           // 查看所有已记录约束

# Job 管理
job.create(intent, worker_id?, depends_on?) // 创建 job，可指定依赖
                                           // worker_id 可选：传则自动 assign，不传则 job 保持 pending
job.status(job_id)                          // 查看 job 状态（轻量：状态枚举 + 基本信息）
job.result(job_id)                          // 获取完整结果（只在 completed 后有意义）
                                           // 返回：result_summary 全文、changed_files、
                                           //       checkpoint 历史、branch 名
job.respond(request_id, answer)             // 回答 worker 的请求（触发 Kingdom 推送给 worker）
job.cancel(job_id)                          // 取消 job（两阶段 shutdown）
                                           // Phase 1：发送 graceful stop，等 30 秒
                                           // Phase 2a：干净停止，git stash 改动
                                           // Phase 2b：30 秒超时则强制 kill，git stash 尽力保存

# 权限管理
worker.grant(worker_id, permission)         // 授权 worker 额外权限
worker.revoke(worker_id, permission)        // 撤销权限

# Failover
failover.confirm(worker_id, new_provider)   // 确认切换
failover.cancel(worker_id)                  // 取消切换，暂停 job
```

---

## Worker 默认工具集（最小权限）

```
# 任务汇报
job.progress(job_id, note)                  // 汇报进度（防超时）
job.complete(job_id, result_summary)        // 标记完成
                                           // result_summary 轻度约定（不强制结构）：
                                           //   完成了什么（具体到文件/功能）
                                           //   改动了哪些文件
                                           //   遗留问题（可选）
                                           // Kingdom 自动附加 changed_files 列表
                                           // 基础校验：不为空、不少于 20 字
job.fail(job_id, reason)                    // 标记失败
job.cancelled()                            // 确认 graceful stop 完成
job.checkpoint(job_id, summary)            // 提交 checkpoint（含五项模板）
job.request(job_id, question, blocking)    // 向 manager 请求指引
                                           // blocking=true：暂停等待 manager 恢复
                                           // blocking=false：继续跑，问题排队等 manager 回来处理
job.request_status(request_id)            // 查询请求是否已有回答（轮询降级模式）

# 只读状态
job.status(job_id)                          // 查看自己的 job 状态

# 按需读取工作区（worker 主动请求，Kingdom 缓存加速）
file.read(path, lines?, symbol?)            // 读取文件内容
                                           // 默认：前 200 行 + 文件结构摘要
                                           // lines="100-300"：读取指定行范围
                                           // symbol="LoginForm"：读取指定函数/类（AST 解析）
workspace.tree(path?)                       // 读取目录结构
git.log(n?)                                 // 读取最近 n 条 commit
git.diff(path?)                             // 读取当前 diff
```

## Worker 初始 Context

Worker 启动时 Kingdom 必传：
- job 描述（用户原始意图）
- checkpoint / 交接简报（接手任务时）
- changed_files 列表（已改动文件，不要重做）

Kingdom 后台预读（worker 请求时立刻返回）：
- README.md
- 与 job 描述相关的文件（Kingdom 根据关键词猜测）

Worker 按需请求其余文件，不一次性全部传入。

---

## Worker 可申请的扩展权限

Manager 可按需授权：

| 权限 | 说明 | 典型场景 |
|---|---|---|
| `subtask.create` | 代替 manager 创建新 job（subtask 就是 job，创建者为该 worker） | 高级 worker 需要拆分工作 |
| `worker.notify` | 通知另一个 worker（经 Kingdom 转发） | 流水线触发下一步 |
| `workspace.read` | 读取全局状态 | Checker 需要看全局进度 |
| `job.read_all` | 读取所有 job（不只是自己的） | 需要了解整体进度的协调型 worker |

---

## Manager 对话模型

Manager 是一个持续运行的 AI 进程，对话界面就是它的工作界面。

- 用户直接在 manager pane 里说话，manager 实时响应
- Kingdom 的 notification 作为普通消息注入 manager 对话流，manager 看到后自动决定下一步
- 用户可以随时在消息流里插话、调整方向，manager 将用户输入和 Kingdom 事件视为同一对话上下文

**Notification 格式（注入 manager 对话的消息）：**

```
[Kingdom] job_001 已完成
  worker: Codex
  摘要: 实现了登录验证，修改了 3 个文件
  changed: src/auth/LoginForm.tsx, src/auth/validation.ts, src/auth/submit.ts
  → 调用 job.result("job_001") 查看完整结果
```

---

## Kingdom → Manager 推送事件

Kingdom 只推送需要 manager 介入的事件，其余 manager 按需查询。

| 事件 | 触发条件 |
|---|---|
| `job.completed` | worker 调用 `job.complete()` |
| `job.failed` | worker 调用 `job.fail()` 或 failover 触发 |
| `job.request` | worker 调用 `job.request()` |
| `job.unblocked` | job 的依赖全部完成，job 变为 `pending` |
| `failover.ready` | Kingdom 已准备好切换，等待 manager 确认 |
| `worker.idle` | worker 完成任务后进入 idle 状态 |

Manager 断连期间事件排队缓存，恢复后 Kingdom 推送摘要。

---

## 通信流向

```
用户
 │
 ▼
Manager（全局视图）
 │  job.assign / worker.grant
 ▼
Kingdom（MCP 服务器，做权限仲裁）
 │  下发对应工具集
 ▼
Worker（受限视图）
 │  job.complete / job.progress
 ▼
Kingdom（记录 action history，通知 manager）
 │
 ▼
Manager（决定下一步）
```

---

## Worker 请求 Manager 指引的完整回路

```
1. Worker 调用 job.request(job_id, question, blocking=true)
2. Kingdom 分配 request_id，通知 manager（推送事件 job.request）
3. Manager 收到通知，调用 job.respond(request_id, answer)
4. Kingdom 存储回答，通知 worker：
   → 支持推送：MCP server→client notification，worker 被唤醒
   → 不支持推送：worker 每 10 秒轮询 job.request_status(request_id)
5. Worker 收到回答，继续工作
```

**blocking=false 时：** worker 继续工作，不等回答；Kingdom 将问题加入待处理队列，manager 恢复后统一处理，回答存储供 worker 后续参考。

**Worker 启动时 Kingdom 告知推送模式：** 在初始 context 里注明 `notification_mode: push | poll`，worker 据此选择等待方式。

---

## Worker 申请提升权限的流程

```
1. Worker 调用 job.request(job_id, "需要创建子任务")
2. Kingdom 通知 manager
3. Manager 决定是否授权，调用 worker.grant(worker_id, "subtask.create")
4. Kingdom 更新该 worker 的工具集
5. Worker 可以调用 subtask.create
```

权限是临时的，job 完成后自动回收。

## `subtask.create` 完整流程

```
subtask.create(intent, depends_on?)
```

- Subtask 本质是 job，创建者记录为该 worker（而非 manager）
- 自动追加 `depends_on=[创建者的 job_id]`（防止游离 job）
- Kingdom 通知 manager：`[Kingdom] worker-1 创建了子任务 job_003: {intent}`
- Manager 决定是否分配 worker 执行，和普通 job 完全一样
- Worker 不能自己给 subtask 分配 worker（无 `worker.assign` 权限）
