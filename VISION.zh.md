# Kingdom v2 设计：Vision

## 核心思路

Kingdom v2 是一个终端原生的多 AI 协作系统。

任何 provider 都可以担任任何角色。工作通过 MCP 协议传递，不依赖屏幕抓取。
失败是系统的一部分，Kingdom 负责让工作在失败后继续，而不是让用户手动重建。

一句话：**用户管意图，Kingdom 管执行连续性。**

---

## 与 v1 的核心差异

| | v1 | v2 |
|---|---|---|
| 角色绑定 | Claude 固定做 manager，Codex/Gemini 做 worker | 任何 provider 可做任何角色 |
| 通信方式 | pane 注入 + 屏幕抓取 | 纯 MCP |
| 完成信号 | worker 写 done.json | MCP tool call |
| 失败处理 | 6 个恢复动词，用户选 | Kingdom 检测 → 用户确认 → 自动替换 |
| Context 管理 | 被动（超限才处理）| 主动压缩 + 结构化传递 |
| 状态可见性 | polling workspace.status | tmux status bar 常驻 + popup 事件通知 |

---

## 产品承诺

当一个 provider 因为网络中断、context 超限、API error 失败时：

1. Kingdom 检测到失败
2. 弹出 popup 说明原因，请求用户确认切换
3. 用户确认后，Kingdom 在**同一个 pane** 启动新 provider
4. 新 provider 收到 Kingdom 压缩好的交接简报，继续工作
5. 用户在 pane 里看到一行 `⚡ HANDOFF: Codex → Claude`，status bar 更新

工作没有中断，上下文没有丢失，用户不需要手动重建任何东西。
