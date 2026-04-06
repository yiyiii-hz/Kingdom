# Kingdom v2 Design: Vision

> Chinese version: [VISION.zh.md](./VISION.zh.md)

## Core Idea

Kingdom v2 is a terminal-native multi-AI collaboration system.

Any provider can take on any role. Work is passed through the MCP protocol and does not depend on screen scraping.
Failure is part of the system. Kingdom’s job is to keep the work going after failure, rather than forcing the user to rebuild it manually.

In one sentence: **The user provides intent, Kingdom provides execution continuity.**

---

## Core Differences from v1

| | v1 | v2 |
|---|---|---|
| Role binding | Claude is fixed as manager, Codex/Gemini as workers | Any provider can take on any role |
| Communication | pane injection + screen scraping | pure MCP |
| Completion signal | worker writes done.json | MCP tool call |
| Failure handling | 6 recovery verbs, user chooses | Kingdom detects → user confirms → automatic replacement |
| Context management | passive (handled only when limits are exceeded) | proactive compression + structured handoff |
| State visibility | polling workspace.status | persistent tmux status bar + popup event notifications |

---

## Product Promise

When a provider fails due to network interruption, context overflow, or an API error:

1. Kingdom detects the failure
2. A popup explains the reason and asks the user to confirm the switch
3. After confirmation, Kingdom starts a new provider in the **same pane**
4. The new provider receives Kingdom’s compressed handoff brief and continues the work
5. The user sees a line in the pane: `⚡ HANDOFF: Codex → Claude`, and the status bar updates

The work is not interrupted, the context is not lost, and the user does not need to manually rebuild anything.
