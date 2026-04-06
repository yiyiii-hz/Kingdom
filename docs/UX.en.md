# Kingdom v2 Design: UX

> Chinese version: [UX.zh.md](./UX.zh.md)

## Tmux Layout

**Main window (`kingdom:main`):** manager + up to 3 workers, 2x2 layout.

```
┌─────────────────────────────────────────────────────────┐
│  kingdom:main                                    [tmux]  │
├──────────────────────┬──────────────────────────────────┤
│  manager             │  worker-1                        │
│  [Claude]            │  [Codex]                         │
│                      │                                  │
│  Planning tasks...   │  Implementing login auth...      │
│                      │                                  │
├──────────────────────┼──────────────────────────────────┤
│  worker-2            │  worker-3                        │
│  [Gemini]            │  [Idle]                          │
│                      │                                  │
│  Writing frontend    │  Waiting for tasks               │
│  styles...           │                                  │
│                      │                                  │
├──────────────────────┴──────────────────────────────────┤
│ [Claude:mgr] [Codex:w1] [Gemini:w2] [idle:w3]  $0.34  12:34 │
└─────────────────────────────────────────────────────────┘
```

**When there are more than 3 workers:** open a new tmux window, with one worker per window.

```
┌─────────────────────────────────────────────────────────┐
│  kingdom:worker-4                                [tmux]  │
├─────────────────────────────────────────────────────────┤
│  worker-4                                               │
│  [Codex]                                                │
│                                                         │
│  Writing unit tests...                                  │
│                                                         │
├─────────────────────────────────────────────────────────┤
│ [Claude:mgr] [Codex:w1] [Gemini:w2] [idle:w3] [Codex:w4]  12:34 │
└─────────────────────────────────────────────────────────┘
```

The status bar shows all workers across windows, and users switch windows with standard tmux shortcuts (`Ctrl+b n`).

The worker limit in the main window can be adjusted in `config.toml` (default: 3):

```toml
[workers]
main_window_max = 3
```

---

## Status Bar (Always Visible at Bottom)

Format: `[provider:role] [provider:role] ...  today’s cost  time`

Example:
```
[Claude:manager] [Codex:impl✓] [Gemini:ui⚠] [idle]  $0.34  14:32
```

Icon meanings:
- No icon: running normally
- `✓`: just finished a task
- `⚠`: attention needed
- `✗`: failed, waiting for handling
- `↻`: switching in progress
- `⏳`: rate limited

## Token Cost Visibility

Use `kingdom cost` to view a detailed breakdown:

```
Today’s cost: $0.34
  Claude   128k tokens   $0.19  ████████░░
  Codex     89k tokens   $0.11  █████░░░░░
  Gemini    45k tokens   $0.04  ██░░░░░░░░

This week: $2.17  This month: $8.43

Most expensive job: job_003 (implement login auth) $0.18
```

Per-provider pricing is built in and can be updated in `.kingdom/config.toml`.

---

## Leave Notifications

Notifications are off by default. Users can configure them by event type in `.kingdom/config.toml`:

```toml
[notifications]
on_job_complete = "none"         # none / bell / system
on_attention_required = "bell"   # when user confirmation is needed (failover, blocking request)
on_job_failed = "system"
```

- `bell`: sends `\a` to the terminal; tmux can forward bells as system notifications (users can configure `set-option -g bell-action any` themselves)
- `system`: uses `osascript` on macOS, `notify-send` on Linux, and silently falls back to a bell if unsupported

---

## Three-Tier Information Architecture

| Layer | Location | Content | Update Frequency |
|---|---|---|---|
| Always visible | status bar | provider + state for each pane | real time |
| Event | popup | switch details, confirmation requests | on event |
| History | pane scrollback | work content + HANDOFF separators | permanent record |

---

## Popup Design

**Switch confirmation popup:**
- Shows the failure reason
- Shows a summary of the handoff brief to confirm the context is complete
- Recommends a replacement provider
- Three actions: confirm switch / choose another provider / pause task

**Popup triggers:**
- Provider failure requires a switch
- Context is about to exceed limits (warn early instead of waiting for failure)
- Job completion (if manager review is needed)

**When popups do not appear:**
- Normal context compression (done silently in the background)
- Status bar updates
- Progress output inside the pane

---

## HANDOFF Separator

Inserted into a pane’s scrollback as a permanent audit record:

```
────────────────────────────────────────────────────
⚡ HANDOFF  Codex → Claude                  14:32:01
Reason: context limit exceeded (98k tokens)
Passed along: first three login verification steps done, form submission handling in progress
────────────────────────────────────────────────────
```

---

## `kingdom log` Output Format

**Default: Job list**

```
$ kingdom log

job_003  ✓  Add unit tests               completed  14:21  Codex    3m12s
job_002  ✓  Write frontend login call    completed  13:45  Gemini   8m04s
job_001  ✓  Implement login auth         completed  12:58  Codex    22m31s
            ↳ failover: Codex→Claude 14:02  context exceeded
```

**`--detail <job_id>`: Full timeline for a single job**

```
$ kingdom log --detail job_001

job_001  Implement login auth
  created   12:58  by manager
  worker    Codex（12:58 → 14:02）
  failover  14:02  context exceeded → Claude
  worker    Claude（14:02 → 13:20）
  completed 13:20  3 files changed
  branch    kingdom/job_001

  checkpoints:
    13:15  [kingdom checkpoint] Verification logic done, form submission in progress
    13:19  [kingdom checkpoint] Form submission complete, ready to write tests
```

**`--actions` : raw action stream (human-readable version of `action.jsonl`)**

```
$ kingdom log --actions

14:21  manager    job.complete   job_003
14:20  worker-2   job.progress   job_003  "All tests passed"
14:02  kingdom    failover       job_001  Codex→Claude
13:15  worker-1   job.checkpoint job_001
```

---

## `kingdom doctor` Diagnostic Output

```
$ kingdom doctor

Checking Kingdom runtime environment...

[System Dependencies]
✓ tmux 3.3a
✓ git 2.42
✗ codex    not installed  → npm install -g @openai/codex
✓ claude   installed

[API Key]
✓ ANTHROPIC_API_KEY    set
✗ OPENAI_API_KEY       not set  → export OPENAI_API_KEY=sk-...
✓ GEMINI_API_KEY       set

[Kingdom Daemon]
✓ daemon running  PID 12345  up 2h34m
✓ MCP socket    /tmp/kingdom/a3f9c2.sock
✓ watchdog      running  PID 12346

[Current Session]
✓ manager    Claude   connected  context 23%
⚠ worker-1  Codex    heartbeat timed out 45s  → kingdom swap worker-1
✓ worker-2  Gemini   connected  context 41%

[Config File]
✓ .kingdom/config.toml    valid
✓ KINGDOM.md              exists
⚠ .kingdom/manager.json  MCP socket path outdated  → kingdom up --refresh-config
```

Each issue includes a concrete fix command, not just an error. When Kingdom is not running, only system dependencies and config files are checked.

---

## Shutdown UX

**No running jobs:**
```
$ kingdom down
✓ Kingdom stopped
```

**Running jobs present:**
```
$ kingdom down

There are 2 running jobs:
  job_001  Implement login auth   [Codex running]
  job_002  Write frontend call     [Gemini running]

[Wait for completion]  [Pause and exit]  [Force exit]
```

- **Wait for completion**: wait until all jobs finish or fail, then stop automatically
- **Pause and exit**: send a checkpoint request to each worker (10 seconds), then stop all processes, marking jobs as `paused`
- **Force exit**: immediately kill all processes, try to preserve git diffs, and mark jobs as `paused`

`kingdom down --force` goes straight to the force-exit path without prompting.

---

## Session Restore UX

When `kingdom up` detects unfinished work, it shows a restore summary:

```
$ kingdom up

Detected unfinished work from the previous session:

  job_001  Implement login auth          [running → Codex paused]
  job_002  Write frontend login call     [waiting → depends on job_001]
  job_003  Add unit tests                [completed ✓]

workspace.notes:
  · Use TypeScript, no `any`
  · Do not add new dependencies under `src/auth/`

Continue from where you left off? [Y/n]
```

After confirmation:
1. Recreate the tmux session
2. Restart the manager provider
3. Restore workers (unfinished jobs resume with checkpoints)
4. The manager receives the restore summary, and the user can simply say "continue"

---

## User Interaction Entry Points

| Action | Way |
|---|---|
| View current status | always visible in the status bar |
| Switch provider | popup confirmation |
| Manually trigger a switch | `kingdom swap <worker>` or `kingdom swap <worker> <provider>` |
| View job history | `kingdom log` |
| Diagnose issues | `kingdom doctor` |
| Start | `kingdom up` |
| Stop | `kingdom down` / `kingdom down --force` |
| Restart daemon | `kingdom restart` (session and provider remain uninterrupted) |
| Return to an existing session | `kingdom attach` |
