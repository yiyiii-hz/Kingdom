# Kingdom v2

> Chinese version: [README.zh.md](./README.zh.md)

Terminal-native AI worker orchestration. Multiple providers, one session, automatic failover.

```
┌──────────────────────┬──────────────────────────────────────┐
│  manager             │  worker-1                            │
│  [Claude]            │  [Codex]                             │
│                      │                                      │
│  Dispatching tasks…  │  Implementing auth middleware…       │
│                      │                                      │
├──────────────────────┼──────────────────────────────────────┤
│  worker-2            │  worker-3                            │
│  [Gemini]            │  [idle]                              │
│                      │                                      │
│  Writing CSS…        │  Waiting for task                    │
│                      │                                      │
├──────────────────────┴──────────────────────────────────────┤
│ [Claude:mgr] [Codex:impl✓] [Gemini:ui⚠] [idle]  $0.34  14:32 │
└─────────────────────────────────────────────────────────────┘
```

Kingdom keeps work running when providers fail. When Codex hits a context limit or drops, Kingdom detects it, asks for confirmation, and hands off to Claude — in the same pane, with a compressed briefing. Work continues without manual rebuilding.

---

## Requirements

- **Rust** 1.75+
- **tmux** 3.0+
- **git** 2.30+
- At least one AI provider CLI installed and authenticated:
  - [claude](https://github.com/anthropics/anthropic-quickstarts) (`ANTHROPIC_API_KEY`)
  - [codex](https://github.com/openai/codex) (`OPENAI_API_KEY`)
  - [gemini](https://ai.google.dev/) (`GEMINI_API_KEY`)

## Installation

```bash
git clone https://github.com/your-org/kingdom-v2
cd kingdom-v2
cargo build --release
cp target/release/kingdom ~/.local/bin/
```

Verify:

```bash
kingdom --help
kingdom doctor
```

`kingdom doctor` checks your environment and tells you exactly what's missing.

---

## Quick Start

**1. Run `kingdom doctor` first**

```bash
$ kingdom doctor

[System]
✓ tmux 3.3a
✓ git 2.42
✓ claude   installed
✗ codex    not found  → npm install -g @openai/codex

[API Keys]
✓ ANTHROPIC_API_KEY    set
✗ OPENAI_API_KEY       not set  → export OPENAI_API_KEY=sk-...

[Kingdom Daemon]
✗ daemon not running
```

Fix anything flagged, then continue.

**2. Start Kingdom**

```bash
cd your-project
kingdom up
```

Kingdom will:
- Create a tmux session named `kingdom`
- Launch the daemon process (with watchdog)
- Open a 2×2 pane layout: manager + up to 3 workers
- Write `.kingdom/` state directory into your project root

If a previous session exists with unfinished jobs, you'll see a resume prompt:

```
Detected unfinished work from last session:

  job_001  Implement auth middleware    [running → paused]
  job_002  Write frontend login call   [waiting]

Resume? [Y/n]
```

**3. Attach to an existing session**

```bash
kingdom attach          # attaches to the default "kingdom" session
kingdom attach myname   # attaches to a named session
```

**4. Stop**

```bash
kingdom down            # graceful — prompts if jobs are running
kingdom down --force    # immediate kill, saves git diff
```

---

## CLI Reference

### Session lifecycle

| Command | Description |
|---|---|
| `kingdom up [workspace]` | Start Kingdom in the given directory (default: `.`) |
| `kingdom down [workspace]` | Stop gracefully; prompts if jobs are running |
| `kingdom down --force` | Kill immediately, save git diff, mark jobs `paused` |
| `kingdom attach [session]` | Attach tmux to a running session (default: `kingdom`) |
| `kingdom restart [workspace]` | Restart the daemon without interrupting providers |

### Workers

```bash
kingdom swap <workspace> <worker-id>             # let Kingdom pick the replacement provider
kingdom swap <workspace> <worker-id> <provider>  # swap to a specific provider
```

### Observability

```bash
kingdom log [workspace]                  # list all jobs
kingdom log [workspace] --detail <id>    # full timeline for one job
kingdom log [workspace] --actions        # raw action stream (action.jsonl)
kingdom log [workspace] --limit <n>      # limit to last N jobs

kingdom cost [workspace]                 # today / weekly / monthly spend by provider

kingdom doctor [workspace]               # environment check with fix hints
```

### Maintenance

```bash
kingdom clean [workspace]           # remove completed job branches
kingdom clean [workspace] --all     # also remove paused/failed branches
kingdom clean [workspace] --dry-run # preview what would be removed
```

### Debugging

```bash
kingdom replay <workspace> <job-id>    # re-run a job's action stream for inspection
kingdom job-diff <workspace> <job-id>  # show the git diff produced by a job
kingdom open <workspace> <target>      # open a job branch in $EDITOR
```

---

## Configuration

Kingdom looks for `.kingdom/config.toml` in your project root. All fields are optional — defaults are shown below.

```toml
[tmux]
session_name = "kingdom"       # tmux session name

[idle]
timeout_minutes = 30           # mark a worker idle after this long with no activity

[health]
heartbeat_interval_seconds = 30   # how often to expect a heartbeat from workers
heartbeat_timeout_count    = 2    # missed heartbeats before flagging as unhealthy
process_check_interval_seconds = 5
progress_timeout_minutes   = 30   # no job.progress after this → ProgressTimeout event

[failover]
window_minutes             = 10   # failure rate window
failure_threshold          = 3    # failures in window before triggering failover
cooldown_seconds           = 30   # wait before allowing another failover
connect_timeout_seconds    = 15   # new provider must connect within this time
swap_checkpoint_timeout_seconds = 10
cancel_grace_seconds       = 30

[notifications]
mode = "poll"                  # "poll" | "push" (push requires webhook)

[webhook]
url     = ""                   # POST target for events (leave empty to disable)
timeout_seconds = 5
events  = ["job.completed", "job.failed", "failover.triggered"]

[cost]
# Per-provider token prices (USD per 1M tokens).
# Update these when provider pricing changes.
claude_input_per_1m  = 3.00
claude_output_per_1m = 15.00
codex_input_per_1m   = 2.50
codex_output_per_1m  = 10.00
gemini_input_per_1m  = 0.075
gemini_output_per_1m = 0.30
```

---

## How It Works

### Components

```
kingdom up
  ├── daemon          Unix socket server, owns session state
  │     ├── MCP server    tools for manager + workers to call
  │     ├── health monitor  heartbeat + process checks per worker
  │     └── failover machine  detects failure → triggers handoff
  ├── watchdog        separate process, restarts daemon if it crashes
  ├── manager pane    one AI provider running in tmux (reads workspace, dispatches jobs)
  └── worker panes    one provider per pane, each connected via MCP
```

### MCP protocol

All communication goes through MCP tool calls over a Unix socket — no screen scraping, no pane injection. Workers call tools like `job.progress`, `job.checkpoint`, and `job.done`; the manager calls `worker.create`, `worker.send`, and `workspace.status`. Kingdom is the source of truth; provider self-reports are not trusted until verified.

### Failover

When a worker fails (heartbeat timeout, process exit, API error):

1. Kingdom detects the failure via the health monitor
2. A tmux popup appears with the failure reason and a compressed briefing of the job state
3. User confirms (or selects a different provider)
4. Kingdom starts the new provider in the same pane
5. The new provider receives the briefing and resumes from the last checkpoint
6. A `⚡ HANDOFF` line is written to pane scroll history as a permanent audit record

The job continues — no context is lost, no manual rebuilding required.

### State on disk

```
.kingdom/
  daemon.pid          running daemon PID
  kingdom.sock        Unix socket (path varies by workspace hash)
  config.toml         your configuration (optional)
  session.json        current session: jobs, workers, manager
  action.jsonl        append-only event log (source of truth for log/replay)
  cost.json           token usage per provider
```

---

## Contributing

```bash
cargo test          # run all tests (215 passing)
cargo clippy        # must be clean before submitting
cargo fmt           # standard formatting
```

PRs should include tests for new behavior. The integration tests in `tests/` spin up real tmux sessions — run them locally before pushing, CI will catch regressions but they're slow.

Bug reports: please include `kingdom doctor` output and `.kingdom/action.jsonl` (redact any secrets).

## License

MIT
