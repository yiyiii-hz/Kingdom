# Kingdom v2 设计：决策记录

## 已决策

| 问题 | 决策 | 理由 |
|---|---|---|
| Q1. 用户怎么发起工作 | 直接在 manager pane 里对话 | 最自然，Kingdom 隐形，manager 决定何时派 worker |
| Q2. 并行 worker 数量 | 用户设上限，manager 在上限内动态控制 | 两层控制，Kingdom 做仲裁 |
| Q3. Worker 间通信 | 全部经过 manager，不直接通信 | 单一协调点，可审计，节省 token |
| Q4. Session 持久化 | B：恢复状态，用户手动确认继续 | 避免意外自动恢复，保留 context 连续性 |
| Q5. `kingdom up` 行为 | 全自动，只问一个问题（默认 manager provider） | 降低首次使用门槛，30 秒内看到系统跑起来 |
| Q6. Checker 角色 | 不引入独立 checker 角色，manager review job 结果即是 check | 减少协议复杂度，manager 本身已有全局视图和 review 能力 |
| Q7. Provider 发现机制 | 启动时主动探测（`which`），config 可覆盖路径；结果存入 session state，failover 推荐时动态参考；worker provider 缺失只警告，manager 缺失才报错 | 允许"无 worker"状态启动，降低门槛；failover 不推荐未安装的 provider |
| Q8. `kingdom swap` CLI | `kingdom swap <worker>` 弹 provider 选择，`kingdom swap <worker> <provider>` 直接切换；先请求 worker checkpoint（10 秒窗口），超时强制切换；走同一套 failover 流程 | 复用 failover 切换逻辑，避免两套实现 |
| Q9. Job 可见性 | Kingdom 只推送需要 manager 介入的事件（完成、失败、worker 请求、依赖解除）；manager 按需调 `workspace.status()`；人类用 `kingdom log` 看历史，status bar 看实时 | 减少噪音，manager 不需要全量推送 |
| Q10. Bootstrap 细节 | MCP config 带 `role` 字段区分 manager/worker，Kingdom 据此下发对应工具集；Kingdom 自动注入标准 manager system prompt，用户可在 `KINGDOM.md` 追加 | 新 provider 接管时无需用户重新描述角色 |
| Q11. `kingdom down` | 有运行中 job 时询问：[等待完成] [暂停并退出] [强制退出]；"暂停并退出"给每个 worker 发 checkpoint 请求（10 秒窗口）后依次停止；无运行中 job 直接退出；`--force` 跳过 checkpoint 立刻 kill | 保护进度，同时提供紧急退出路径 |
| Q12. 多 workspace | 每个 workspace 独立 daemon，不共享；socket 按 workspace 路径 hash 隔离 | 隔离性最重要，多进程可接受，实现最简单 |
| Q13. Failover 误触发防护 | 心跳间隔 30 秒，连续 2 次未响应（60 秒）才触发 failover；进程 exit 即时触发，不等心跳；阈值可在 config.toml 调整 | 区分卡死和崩溃，避免网络抖动误触发 |
| Q14. Worker pane 直接打字 | 放开交互（保留逃生口价值）；worker 启动时 pane 顶部显示一行静态提示"直接输入不会被记录，仅用于紧急干预"，之后不再提示 | 不强制锁定，告知用户边界即可 |
| Q15. `KINGDOM.md` 格式 | 纯自由 Markdown，Kingdom 整段传给 provider；不做结构化解析；机器配置放 `.kingdom/config.toml`，AI 行为约束放 `KINGDOM.md` | 约束是给 AI 读的，自由文本比结构化更清晰；职责分离 |
| Q16. `job.request()` 回路 | Manager 用 `job.respond(request_id, answer)` 回答；优先走 MCP server→client notification 推送给 worker；provider 不支持推送时降级为 worker 每 10 秒轮询 `job.request_status(request_id)`；Kingdom 在 worker 启动时告知用哪种模式 | 推送更干净，轮询作为兜底保证兼容性 |
| Q17. Failover 时文件破损 | 不自动回滚，把判断权交给新 provider；handoff 简报新增一项"以下文件在崩溃时正在写入，可能不完整，请先检查" | AI 能判断代码状态；自动回滚丢失已完成工作，语法检查覆盖不了逻辑问题 |
| Q18. API key 管理 | Kingdom 不存储、不加密、不注入 key；`kingdom up` 时检测环境变量，缺失时给出明确提示和设置命令；用户自己管理 key | 零安全风险，兼容所有已有 key 管理方案（shell profile / direnv / 1Password CLI）|
| Q19. 并发 worker 上限 | 默认 3 个 worker 在主 window 里；manager 需要更多时，超出部分在新 tmux window 里启动；status bar 跨 window 显示所有 worker；上限可在 config.toml 调整 | 主 window 布局干净，扩展无上限，用户用标准 tmux 切换 |
| Q20. 离开时通知 | 默认不发通知；用户可在 config.toml 按事件类型配置 none / bell / system；`on_attention_required` 建议默认 bell | 不骚扰，用户按需开启；bell 跨平台，system 通知 macOS/Linux 各自实现 |
| Q21. Git 策略与 failover | Failover 时新 provider 在同一个 branch 继续；checkpoint 时自动 commit（`[kingdom checkpoint]`），保证每个 checkpoint 有干净的 diff；`job.complete` 时不自动 commit，manager 决定后续 | checkpoint commit 解决累积 diff 问题；job 完成后的 merge/squash/丢弃由 manager 判断 |
| Q22. `job.complete` result_summary | 轻度约定（完成了什么、改动了哪些文件、遗留问题可选）；Kingdom 基础校验（不为空、≥20字）；Kingdom 自动附加 changed files 列表；不强制结构化 | job 完成时 worker 状态健康，无需强制约束；manager 是 AI，自由文本足够 |
| Q23. `kingdom doctor` | 检查五个层面：系统依赖、API key、daemon 状态、session 连接健康、配置文件；每个问题给出具体修复命令 | 出问题时第一个跑的命令，必须可操作，不只是报错 |
| Q24. Provider 断线重连 | Provider→Kingdom：指数退备（1s起，30s封顶），无限重试；重连期间 tool call 本地缓存，Kingdom 恢复后补报；Kingdom→Provider（重启后）：主动重建连接，15s 超时无响应则标记离线触发 failover | Kingdom 总会回来所以无限重试；进程消失由 PID 监控处理，不走重连路径 |
| Q25. Manager notification 机制 | notification 作为普通消息注入 manager 对话流；manager（AI）看到消息自动决定下一步；用户可随时插话；不使用独立 notification channel | 对话就是 manager 的工作界面；独立 channel 需要各 provider 额外实现，复杂度高 |
| Q26. `kingdom up` 遇到已有 session | daemon+session 都在：提示 attach 或 restart；daemon 在但 session 丢了：自动重建 session 恢复状态；session 名冲突：报错提示；session 名可在 config.toml 配置（默认 `kingdom`）| 三种情况分别处理，避免意外覆盖 |
| Q27. Manager context 超限 | 不做摘要压缩，超限直接触发 failover；新 Manager 从 Kingdom 结构化状态重建（job 状态、workspace.notes、action log），不依赖对话历史 | Manager 真正的"记忆"在 Kingdom 结构化状态里；对话历史是过程不是状态；复用 Manager failover 机制，无需新机制 |
| Q28. Worker idle 复用 | 不复用；idle 超时（默认 10 分钟）后直接终止进程；Manager 需要新 worker 时走 `worker.create()`，新进程新 context | 冷启动代价极低（秒级）；带旧 context 复用风险高（污染 + 提前超限）|
| Q29. `subtask.create` 流程 | subtask 就是 job，创建者是 worker；自动 `depends_on` 创建者的 job；Kingdom 通知 manager，manager 决定是否分配 worker 执行；worker 不能自己分配 worker | 复用 job 机制，不引入新概念；manager 保持对所有执行的控制权 |
| Q30. Job 取消级联 | 取消 job_A 时 Kingdom 检查依赖方，若有则弹出询问：[一并取消] [保持等待] [逐个决定]；默认不自动级联 | 取消是主动决定，级联语义与失败不同；保持 manager 对每个 job 的控制权 |
| Q31. `kingdom log` 格式 | 三个视图：默认 job 列表（状态/时间/provider/耗时）；`--detail <job_id>` 单个 job 完整时间线；`--actions` 原始操作流（action.jsonl 的可读版）| 覆盖"发生了什么"的两个场景：job 历史和操作审计 |
| Q32. `kingdom restart` | 只重启 daemon，不重建 tmux session，不重启 provider；SIGTERM→5s→SIGKILL；daemon 从 `.kingdom/` 恢复状态后重连所有存活 provider | 适用于 config 更新生效、daemon 异常热重置；与 down+up 的区别是 session 和 provider 不中断 |
| Q33. `job.progress` 超时 | 默认 30 分钟无上报触发警告（不自动 failover）；弹出询问：[等待] [请求 checkpoint] [触发切换]；可在 config.toml 配置；与心跳超时（60s）独立 | 长任务静默是正常的（如跑大型测试）；用户最了解当前任务是否真的卡住 |
| Q34. Worker 命名约定 | 格式 `w{序号}`（w1, w2...）；session 内不回收（日志里同名始终指同一 worker）；跨 session 重置（kingdom up 重新从 w1 开始）；status bar 显示 `[Codex:w1]`，CLI 引用 `kingdom swap w1` | session 内不回收保证审计清晰；跨 session 重置保证数字不膨胀 |
| Q35. Provider 启动参数 | Kingdom 内置三个 provider 的启动模板（claude/codex/gemini）；`{mcp_config}` 占位符自动替换；用户可在 config.toml 覆盖 binary 路径和 args；自定义 provider 需用户自己配 args | v2 只内置三个主流 provider，其余开放配置 |
| Q36. `worker.release` 语义 | 只能 release idle worker（有 job 在跑则报错，要求先 cancel）；release = 立刻终止进程，无需 checkpoint；是 idle 超时的手动提前版 | idle worker 无进行中工作，无需 graceful stop |
| Q37. `kingdom clean` 设计 | 显示将清理的内容和释放空间，用户确认后执行；支持 `--dry-run`（只显示）和 `--all`（不按时间限制）；清理三类：中间 checkpoint(>7天)、旧 job 结果(>90天)、旧 action log(>30天，压缩为摘要）| 清理前完整告知，不静默删除 |
| Q38. `workspace.note` 冲突检测 | 不做自动检测；Manager 每次接手时读取全部 note，自己判断冲突并清理；Kingdom 不跑 LLM 检测 | Manager（AI）的语义理解比关键词检测更可靠；省去额外 LLM 调用的成本和延迟 |
| Q39. `.kingdom/` gitignore | 在 `.kingdom/` 内创建 `.gitignore`（内容 `*`）；`kingdom up` 初始化时自动创建；不修改项目根目录的 `.gitignore` | 效果等同于 gitignore，但不改动用户文件 |
| Q40. `workspace.notes` 持久化 | 跨 session 持久化，存在 `.kingdom/state.json`；`kingdom down` 不清除；`kingdom up` 恢复时随 job 状态一起读取；用户可手动将重要 note 写进 `KINGDOM.md` 永久化 | notes 是动态捕获的约束，与用户手写的 `KINGDOM.md` 来源不同，分开存储 |
| Q41. 非 git 目录 | 检测到非 git 仓库时警告并询问是否继续；自动降级到 `strategy = "none"`；checkpoint 只保存文字摘要，无 diff 快照；不强制要求 git | 有些场景不需要 git（临时脚本、文档工作）；降级比报错退出更友好 |
| Q42. `job.result()` 遗漏 | 补入 Manager 工具列表；返回 result_summary 全文 + changed_files + checkpoint 历史 + branch 名；与 `job.status()` 区分：status 轻量实时，result 完整只在 completed 后有意义 | 补漏，完善 Manager 工具集 |
| Q43. 多人共享 workspace | 不支持同一目录多实例；第二个 `kingdom up` 检测到 socket 已占用，提示 `kingdom attach`；各人在自己机器上跑 Kingdom 完全隔离（`.kingdom/` 已 gitignore）| 一个 workspace 一个 Kingdom 实例，保证单一控制点 |
| Q44. `job.create` + `worker.assign` 合并 | `job.create` 加可选 `worker_id` 参数；传则自动 assign，不传则 job 保持 `pending`；不引入新工具，不破坏现有接口 | 保留规划/执行分离的灵活性，同时支持一步派发 |
