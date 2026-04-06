# Kingdom v2 Design: Architecture

## Core Architecture

```
┌─────────────────────────────────────────────────┐
│  Kingdom Core                                   │
│                                                 │
│  ┌──────────────┐    ┌──────────────────────┐   │
│  │  MCP Server  │    │  Process Manager     │   │
│  │              │    │                      │   │
│  │  Tool Permission Arbitration │    │  PID Tracking        │   │
│  │  Action Logging              │    │  Health Monitoring   │   │
│  │  Context Tracking            │    │  Failover Trigger    │   │
│  └──────┬───────┘    └──────────┬───────────┘   │
│         │                       │               │
└─────────┼───────────────────────┼───────────────┘
          │ MCP (socket)          │ PID monitor
          │                       │
    ┌─────▼───────────────────────▼──────┐
    │         Provider Process           │
    │                                    │
    │  ┌─────────────┐  ┌─────────────┐  │
    │  │ MCP Client  │  │     tty     │  │
    │  │ (tool calls)│  │  (display + interaction) │  │
    │  └─────────────┘  └──────┬──────┘  │
    └──────────────────────────┼─────────┘
                               │
                         tmux pane
                     (visible to user + interactive)
```

---

## Security Model

**Prompt Injection Defense: Two-Layer Mechanism**

Instead of trying to identify malicious instructions in file contents, which is too hard, limit what the worker can do:

**Layer 1: Least Privilege**
- By default, the worker only has the minimal tool set, with no dangerous tools such as `shell.exec` or `workspace.delete`
- Even if it gets injected, the damage it can cause is very limited

**Layer 2: Kingdom Intercepts Abnormal Tool Calls**
- All tool calls pass through Kingdom, which checks whether they exceed the authorized scope
- If something abnormal happens, Kingdom blocks it immediately and alerts:

```
⚠ Abnormal operation blocked
  Worker job_001 attempted to call unauthorized tool: shell.exec("rm -rf .kingdom/")
  Possible reason: the file being read contains malicious instructions
  [Ignore]  [Pause worker]  [Terminate worker]
```

---

## Storage Management

**Tiered retention policy:**

| Data type | Retention policy |
|---|---|
| Checkpoints for unfinished jobs | Kept forever |
| `workspace.notes` | Kept forever |
| Action logs from the last 30 days | Kept forever |
| Final results for completed jobs | Archived after 90 days |
| Intermediate checkpoints for completed jobs | Keep only the last one after 7 days |
| Detailed action logs older than 30 days | Compressed into summaries |

Retention periods can be adjusted in `.kingdom/config.toml`.

`kingdom clean` manually triggers cleanup. Before cleaning, it shows what will be deleted and how much space will be freed, then runs after user confirmation:

```
$ kingdom clean

The following will be cleaned:

  Intermediate checkpoints for completed jobs (>7 days)
    job_001  3 checkpoints  · 2.1 MB  2026-03-15
    job_002  5 checkpoints  · 4.8 MB  2026-03-18

  Archive completed job results (>90 days)
    job_047  · 1.2 MB  2025-12-10

  Compress old action logs (>30 days)
    2026-02-01 ~ 2026-03-05  · 18 MB → about 0.5 MB

Total freed: about 26 MB

[Confirm cleanup]  [Cancel]
```

`kingdom clean --dry-run`: show only, do not execute.  
`kingdom clean --all`: clean all cleanable content without time limits (use with caution).

---

## Data Consistency

**All write operations go through Kingdom; the worker does not write directly to the file system.**

```
Worker → job.checkpoint() → MCP → Kingdom queue → serialized disk write
Worker → job.complete()   → MCP → Kingdom queue → serialized disk write
Worker → job.progress()   → MCP → Kingdom queue → serialized disk write
```

Kingdom is a single process handling all MCP requests, so writes are naturally serialized with no race conditions.

**Directory structure (each job isolated independently):**

```
.kingdom/
  state.json              global state (exclusively written by Kingdom)
  jobs/
    {job_id}/
      meta.json           job metadata
      checkpoints/        checkpoint history
      handoff.md          latest handoff brief
      result.md           final result
  logs/
    action.jsonl          full action log (append-only)
```

---

## Two Independent Channels

### MCP Channel (Kingdom-only)
- The provider connects to Kingdom's MCP server
- All structured communication goes here: job reporting, permission requests, status updates
- Kingdom uses this channel to obtain authoritative information and does not read stdout

### TTY Channel (human-only)
- The provider process's tty is bound to the tmux pane
- The user can directly see what the AI is doing
- When manual intervention is needed (sudo password, urgent confirmation), the user types directly in the pane
- The pane is a **display window + interaction escape hatch**, not a source of information for Kingdom

**Worker pane interaction boundary:**

When the worker starts, a static line is shown at the top of the pane and is not repeated afterward:

```
[Kingdom] Input typed directly here will not be recorded in action history and is only for emergency intervention
```

Anything the user types directly into the worker pane is not visible to Kingdom, and if failover happens, that context is lost. This is intentional: the pane is an escape hatch, not a formal collaboration channel.

---

## Provider Startup Flow

```
manager calls worker.create("codex")
       ↓
Kingdom Process Manager
  1. tmux split-window creates a new pane
  2. Start in the pane: codex --mcp-config worker.json
     (the process tty is bound to the pane, keeping it interactive)
  3. Record the PID and begin monitoring
       ↓
Codex process starts
  1. MCP client connects to the Kingdom MCP server (separate socket)
  2. Automatically discovers the worker tool set
  3. Receives initial context (job description + workspace info)
  4. Starts working, output is shown in the pane
       ↓
Kingdom
  1. Confirms connection via MCP heartbeat
  2. Starts tracking token usage
  3. Updates the status bar
```

---

## Health Monitoring

Kingdom monitors each provider across two dimensions:

| Dimension | Method | Failure condition |
|---|---|---|
| Process liveness | PID monitoring | Process exits abnormally, triggers immediately |
| MCP connectivity | Heartbeat ping (every 30 seconds) | No response for 2 consecutive checks (60 seconds) before triggering |
| Context health | Token tracking | Exceeds threshold (70%) |
| Task responsiveness | `job.progress` interval | No report for 30 minutes by default → warning (no automatic failover); prompt the user |

Process exit and heartbeat timeout are handled separately: process exit is a definite crash and triggers immediately; heartbeat timeout suggests a hang and needs a longer confirmation window to avoid false positives.

**Configurable items (`.kingdom/config.toml`):**
```toml
[health]
heartbeat_interval_seconds = 30
heartbeat_timeout_count = 2         # number of missed responses before triggering failover
progress_timeout_minutes = 30       # how long without `job.progress` before warning (no automatic failover)
```

Any one dimension triggering → enter failover flow.

---

## MCP Socket Management

The Kingdom MCP server uses a Unix domain socket and generates a unique address based on the workspace path:

```
/tmp/kingdom/{workspace_hash}.sock
```

- `workspace_hash` = hash of the repo root path (for example, `a3f9c2`)
- Each workspace has its own independent socket, so multiple workspaces can run at the same time without interfering
- No port conflicts, no firewall issues

`kingdom up` automatically generates the MCP config and writes the socket path:

```json
{
  "mcpServers": {
    "kingdom": {
      "transport": "unix",
      "socket": "/tmp/kingdom/a3f9c2.sock"
    }
  }
}
```

After Kingdom restarts, the socket file is recreated and the provider reconnects automatically.

---

## Kingdom Reliability

**Each workspace runs its own Kingdom daemon, isolated from the others.**

The Kingdom daemon runs as a daemon and is automatically restarted if it crashes:

```
kingdom up
  ↓
Start a watchdog process (lightweight, only responsible for monitoring and restarting the Kingdom daemon for this workspace)
  ↓
watchdog starts the Kingdom daemon
  ↓
When the Kingdom daemon crashes, the watchdog restarts it immediately
```

When multiple workspaces run at the same time, each has its own independent daemon + watchdog + socket, and they do not affect one another.

**State persistence principle:** write to disk immediately after every operation; do not depend on in-memory state. After Kingdom restarts, it fully restores from `.kingdom/`.

**Recovery flow after restart:**
1. Read `.kingdom/` to restore workspace, job, and worker state
2. Re-establish MCP connections to all still-alive providers
3. Restore token tracking counters
4. Restore the status bar display

**Provider-side reconnection:** after an MCP connection drops, retry with exponential backoff (1s → 2s → 4s → … → capped at 30s), retrying indefinitely until Kingdom returns. Tool calls are cached locally during reconnection and reported back once Kingdom is restored. Provider process exits are handled by PID monitoring and do not go through the reconnection path.

**Active reconnection after Kingdom restart:** after Kingdom reads `.kingdom/` and restores state, it proactively re-establishes MCP connections to all known providers. If there is no response within 15 seconds, the provider is marked offline and the failover flow is triggered.

---

## Kingdom Startup Order

**Behavior when an existing session is present:**

| Situation | Handling |
|---|---|
| daemon and session are both running | Prompt `kingdom attach` to return to the existing session or `kingdom restart` to restart |
| daemon is running, but the session is lost | Automatically recreate the tmux session and restore all job state |
| session name conflict (non-Kingdom session) | Return an error and guide the user to configure a different session name in `config.toml` |

```toml
[tmux]
session_name = "kingdom"    # customizable to avoid conflicts
```

---

```
kingdom up
  ↓
1. Check tmux (if missing, error and guide installation)
   Check git (if missing, warn and ask whether to continue in no-git mode; after confirmation, automatically downgrade `strategy = "none"`)
2. Initialize the `.kingdom/` directory (skip if it already exists), create `.kingdom/.gitignore` (content `*`)
3. Detect available providers (which claude / codex / gemini ...)
   → Store the result in session state
   → If the manager provider is unavailable: exit with an error
   → If the worker provider is unavailable: warn and continue startup
   Check API key environment variables and prompt if missing (does not block startup):
   ```
   ✗ OPENAI_API_KEY is not set (Codex unavailable)
     → export OPENAI_API_KEY=sk-...
   ```
   Kingdom does not store or inject API keys; provider processes inherit the current shell environment variables.
4. Generate MCP config (manager.json + worker.json)
5. Start the Kingdom MCP server (background daemon)
6. Ask for the default manager provider (only list detected available options)
7. Create the tmux session
8. Start the manager provider in pane-0
9. Wait for the manager MCP connection to succeed
10. Output: ✓ Startup complete
```

---

## MCP Config Structure

`kingdom up` generates two MCP config files, and the `role` field determines which tool set Kingdom provides:

```json
// .kingdom/manager.json
{
  "mcpServers": {
    "kingdom": {
      "transport": "unix",
      "socket": "/tmp/kingdom/a3f9c2.sock",
      "role": "manager",
      "session_id": "sess_abc123"
    }
  }
}

// .kingdom/worker.json
{
  "mcpServers": {
    "kingdom": {
      "transport": "unix",
      "socket": "/tmp/kingdom/a3f9c2.sock",
      "role": "worker",
      "session_id": "sess_abc123"
    }
  }
}
```

The worker config adds `job_id` dynamically when `worker.create()` is called, and Kingdom uses it to associate tool calls with a specific job.

## Manager Initial Prompt

After the provider connects, Kingdom injects the standard manager system prompt via MCP:

```
You are Kingdom's manager.
Current workspace: {path}
Available worker providers: {available_providers}
Current job state: {workspace.status snapshot}

Your responsibilities: analyze user intent, split tasks, dispatch to workers, and review results.
Interact with Kingdom through MCP tools; do not operate on the file system directly.
```

Any content the user appends in `KINGDOM.md` is appended after the standard prompt and has higher priority.

---

## Provider Discovery

**Detection method:** actively probe with `which <provider_binary>`, and `.kingdom/config.toml` can override the path.

**Detection results:**
- Stored in session state (`available_providers`)
- Failover recommendations only suggest available providers
- `kingdom up` prints a detection summary:

```
✓ claude   installed (/usr/local/bin/claude)
✓ codex    installed (/usr/local/bin/codex)
✗ gemini   not found (optional)
```

**Provider startup templates (built in, overridable):**

Kingdom includes startup arguments for three providers. `{mcp_config}` is an automatically substituted placeholder for the config path:

```toml
[providers.claude]
binary = "claude"
args = ["--mcp-config", "{mcp_config}"]

[providers.codex]
binary = "codex"
args = ["--mcp-config", "{mcp_config}"]

[providers.gemini]
binary = "gemini"
args = ["--mcp-config", "{mcp_config}"]
```

Users can override any field in `.kingdom/config.toml` and append custom arguments:

```toml
[providers.codex]
args = ["--mcp-config", "{mcp_config}", "--model", "gpt-4o"]

[providers.gemini]
binary = "/opt/custom/gemini-cli"
```

v2 includes only the built-in `claude` / `codex` / `gemini` providers; custom providers must be configured manually with a complete `args` list.