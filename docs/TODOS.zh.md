# TODOS

> English version: [TODOS.en.md](./TODOS.en.md)

## 工程评审（plan-eng-review，2026-04-06）

### 分发与 CI/CD 流水线

**What:** 添加 CI/CD 流水线，用于构建并发布 kingdom 二进制文件（linux-x86_64、linux-aarch64、macos-aarch64）。

**Why:** BUILD_PLAN 里没有 `.github/workflows/`，这意味着任何人都只能从源码构建，无法直接安装 Kingdom。对于早期使用者来说，预构建二进制文件是基本配置。

**Context:** 该计划此前将此项推迟为“超出 M1-M10 范围”。等 M10 完成且 CLI 稳定后，添加 GitHub Actions 矩阵构建 + 发布步骤，产出静态二进制文件和 `CHECKSUMS` 文件。使用 `cross` crate 做交叉编译。典型模式是：`push to main` 触发构建，`push tag v*` 触发发布。

**Effort:** M  
**Priority:** P2  
**Depends on:** M10 complete

---

## 基础设施

### Daemon.pid 崩溃后的生命周期

**What:** 确保 `.kingdom/daemon.pid` 在 daemon 正常退出时被删除，并且 watchdog 在重启时能处理陈旧的 pid 文件。

**Why:** 如果 daemon 崩溃但 pid 文件仍然保留，下一次 `kingdom up` 可能会莫名其妙地提示“daemon already running”，即使它其实并没有在运行。

**Context:** M5 规范说明 daemon 会在启动时写入 `daemon.pid`。还需要补上：(a) 在 daemon 的 shutdown 处理器中删除它，(b) 让 `kingdom up` 在把它当作“running”之前，先检查文件里的 PID 是否真的还活着。该项已加入 M5 的验收标准（启动时写入 `daemon.pid`），但陈旧 PID 检测还没有被明确写进去。

**Effort:** S  
**Priority:** P1  
**Depends on:** M5

---

### CLI Socket 竞态窗口

**What:** 处理 daemon 正在启动、socket 还没创建完成时到达的 CLI 查询。

**Why:** 在 `kingdom up` 之后立刻运行 `kingdom log` 或 `kingdom doctor`，可能会撞上 CLI socket 还不存在的窗口期。

**Context:** M2 增加了 CLI socket（`{hash}-cli.sock`），但没有规定当 CLI 命令在 socket 就绪前到达时该怎么办。CLI 应该最多重试 3 秒，采用 200ms 退避，然后再报告“daemon not ready”。

**Effort:** S  
**Priority:** P2  
**Depends on:** M2

---

### Headless 模式（--no-tmux）

**What:** `kingdom up --no-tmux` 让 Kingdom 在没有 tmux 的环境中运行（CI/CD、远程服务器）。

**Why:** 现在整个系统默认假设 tmux 可用，没有 tmux 时 `kingdom up` 会直接报错。CI/CD 用户无法用 Kingdom 自动化工作流。

**Context:** 实现思路：在 M5 的 `kingdom up` 中加入 `--no-tmux` flag，跳过 tmux session 创建，让 provider 进程的 stdin/stdout 直接重定向到 `.kingdom/logs/{worker_id}.log`。Status bar 降级为定期输出 stdout 状态行。Popup 确认降级为 CLI 输入。M8 的 tmux 功能全部退化为日志输出。

**Effort:** L（human: ~1周 / CC+gstack: ~1天）  
**Priority:** P3  
**Depends on:** M10 complete

---

## 已完成

*(none yet)*
