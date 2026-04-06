# TODOS

> Bilingual versions: [English](./TODOS.en.md) | [中文](./TODOS.zh.md)

## Engineering Review (plan-eng-review, 2026-04-06)

### Distribution & CI/CD Pipeline

**What:** Add CI/CD pipeline to build and publish kingdom binary (linux-x86_64, linux-aarch64, macos-aarch64).

**Why:** The BUILD_PLAN has no `.github/workflows/` — no one can install Kingdom without building from source. Pre-built binaries are table stakes for any early adopter.

**Context:** Plan deferred this as "out of scope for M1-M10." Once M10 is done and the CLI is stable, add a GitHub Actions matrix build + release step that produces static binaries and a CHECKSUMS file. Use `cross` crate for cross-compilation. Typical pattern: `push to main` triggers build, `push tag v*` triggers release.

**Effort:** M
**Priority:** P2
**Depends on:** M10 complete

---

## Infrastructure

### Daemon.pid Lifecycle on Crash

**What:** Ensure `.kingdom/daemon.pid` is deleted on clean daemon exit, and watchdog handles stale pid files on restart.

**Why:** If daemon crashes and pid file remains, the next `kingdom up` may confusingly report "daemon already running" when it isn't.

**Context:** M5 spec says daemon writes `daemon.pid` on startup. Need to also: (a) delete it in the daemon's shutdown handler, (b) have `kingdom up` check if the PID in the file is actually alive before treating it as "running." Added to M5 acceptance criteria (`daemon.pid` written on startup) but the stale-PID detection is not yet explicitly specified.

**Effort:** S
**Priority:** P1
**Depends on:** M5

---

### CLI Socket Race Window

**What:** Handle CLI queries that arrive when daemon is starting (before socket is created).

**Why:** `kingdom log` or `kingdom doctor` run right after `kingdom up` may hit a window where the CLI socket doesn't exist yet.

**Context:** M2 added the CLI socket (`{hash}-cli.sock`) but doesn't specify what happens if a CLI command arrives before the socket is ready. CLI should retry for up to 3 seconds with 200ms backoff before reporting "daemon not ready."

**Effort:** S
**Priority:** P2
**Depends on:** M2

---

### Headless 模式（--no-tmux）

**What:** `kingdom up --no-tmux` 让 Kingdom 在无 tmux 的环境中运行（CI/CD、远程服务器）。

**Why:** 现在整个系统假设 tmux 可用，没有 tmux 时 `kingdom up` 直接报错。CI/CD 用户无法使用 Kingdom 自动化工作流。

**Context:** 实现思路：M5 的 kingdom up 加 `--no-tmux` flag，跳过 tmux session 创建，provider 进程 stdin/stdout 直接管道到 `.kingdom/logs/{worker_id}.log`。Status bar 退化为定期 stdout 状态行。Popup 确认退化为 CLI 输入。M8 的 tmux 功能全部降级到日志输出。

**Effort:** L（human: ~1周 / CC+gstack: ~1天）
**Priority:** P3
**Depends on:** M10 complete

---

## Completed

*(none yet)*
