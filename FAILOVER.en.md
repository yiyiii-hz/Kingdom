# Kingdom v2 Design: Failover

## User Manual Intervention

When the user directly presses `Ctrl+C` or runs `tmux kill-pane`, Kingdom determines behavior by exit code:

```
exit 0              → user exited intentionally, job marked paused, no failover triggered
exit != 0           → provider crashed, trigger failover flow
tmux kill-pane      → process disappears, treat as a crash
```

**5-second grace period:**

After the process exits, Kingdom waits 5 seconds and shows a popup:

```
┌─────────────────────────────────┐
│  Codex process has exited       │
│                                 │
│  Did you stop it manually?      │
│                                 │
│  [Yes, pause task] [No, failover]│
└─────────────────────────────────┘
```

- User confirms "Yes" → job marked `paused`, no failover triggered
- User confirms "No" or gives no response within 5 seconds → treat as a crash and trigger failover

---

## Detection

Kingdom continuously monitors each provider process through MCP heartbeats and process state.

Trigger conditions:
- Network interruption: MCP connection disconnected for more than N seconds
- Context limit exceeded: API returns a context length error
- Process exit: provider process exits abnormally
- Timeout: no `job.progress` / `job.complete` response beyond the configured time

---

## Switchover Flow

```
1. Kingdom detects a failure
       ↓
2. Kingdom pauses output for that pane
       ↓
3. tmux display-popup opens a confirmation dialog
   ┌─────────────────────────────────┐
   │ ⚠️  Codex failed                │
   │ Reason: Context limit exceeded (98k tokens) │
   │                                 │
   │ Handoff brief is ready:         │
   │ · Done: first three login verification steps │
   │ · In progress: form submission handling │
   │ · Remaining: error message UI   │
   │                                 │
   │ Switch to: Claude (recommended) │
   │                                 │
   │ [Confirm switch] [Choose another] [Cancel] │
   └─────────────────────────────────┘
       ↓ user confirms
4. Kingdom starts the new provider in the same pane
       ↓
5. The pane prints a separator line:
   ────────────────────────────────────────
   ⚡ HANDOFF  Codex → Claude
   Reason: Context limit exceeded (98k tokens)
   Transferred: login verification in progress, form submission remaining
   ────────────────────────────────────────
       ↓
6. The new provider receives the handoff brief and continues work
       ↓
7. The status bar updates to show the new provider
```

---

## Manager Failure

The same flow as worker failure.

Special case: when the manager fails, all workers stop accepting new tasks and wait for the new manager to take over before continuing.

---

## Rate Limit Handling

Rate limit (429) does not trigger failover and is handled separately:

```
rate_limit detected
      ↓
Exponential backoff retries: 5s → 15s → 30s → 60s
status bar shows: [Codex:impl ⏳ rate limited]
      ↓
Retry succeeds → resume normal work
Retry fails 3 times → degrade to crash handling and trigger failover
```

---

## Circuit Breaker

Prevents infinite cascades of repeated failures.

**Trigger condition:** the same job fails 3 or more times within 10 minutes

**After triggering:**
- Job is marked `paused`
- Failover is no longer triggered automatically
- Popup notifies the user: `job_001 failed 3 times in a row, please inspect the task or manually choose the next step`
- Status bar shows: `[job_001 ⛔]`

**Cooldown:** at least 30 seconds between two failovers to prevent a new provider from being misclassified as failed before it stabilizes

**Configurable options (`.kingdom/config.toml`):**
```toml
[failover]
window_minutes = 10      # time window
failure_threshold = 3    # max failures within the window
cooldown_seconds = 30    # minimum interval between failovers
```

---

## Provider Startup Failure

Startup failures are handled differently from runtime crashes; they do not trigger failover:

| Case | Behavior | Handling |
|---|---|---|
| binary not found | error on startup | tell the user to install it, job paused |
| crashes immediately after startup | process lives for less than 3 seconds | tell the user to check config, job paused |
| starts but never connects to MCP | 15-second timeout | kill the process, inform the user, job paused |
| runtime crash | process exits (`exit != 0`) | trigger failover |

Startup failures always return actionable error messages:
```
binary not found  → "Please install codex first: npm install -g @openai/codex"
crashes on launch → "codex failed to start (exit 1), please check the configuration"
MCP connection timeout → "codex started successfully but did not connect to Kingdom, please check the MCP configuration"
```

---

## Failover UX During the Gap

Between the old provider crashing and the new provider connecting successfully, the pane shows a transition state:

```
────────────────────────────────────────────────
⚡ HANDOFF  Codex → Claude                14:32:01
Reason: Context limit exceeded (98k tokens)
────────────────────────────────────────────────
⏳ Starting Claude... (3s)
```

- The timer updates every second so the user knows the system is working
- Once startup succeeds, the transition line disappears and new provider output begins
- If no connection is established after 15 seconds, show a warning and offer actions:
  ```
  ⚠ Claude startup timed out
  [Retry]  [Choose another provider]  [Pause task]
  ```

---

## Recommended Replacement Providers

Kingdom recommends providers in this order:

1. Another available provider of the same type (for example, if Codex fails, recommend another Codex instance)
2. The closest-capability provider, based on the built-in capability preference table
3. Manual user selection

The user can override the recommendation and choose any available provider.

---

## Manager Failover

When the manager fails, the new manager restores from Kingdom’s structured data and does not rely on conversation history.

**The full bundle received when the new manager starts:**

```
1. KINGDOM.md          engineering constraints (tech choices, code conventions, architecture decisions)
2. CLAUDE.md           behavioral constraints (output format, language style)
3. workspace.notes     session constraints (captured by manager via workspace.note())
4. All job state       current progress, checkpoints, assigned workers
5. Pending queue       paused jobs, blocking requests, questions awaiting confirmation
6. The most recent N action logs to understand what just happened
```

**`KINGDOM.md` format:** plain free-form Markdown. Kingdom passes the whole section to the provider as part of the system prompt without structured parsing. Machine behavior config lives in `.kingdom/config.toml`, AI behavior constraints live in `KINGDOM.md`; responsibilities are separated.

**Priority:** KINGDOM.md (engineering constraints) > CLAUDE.md (behavioral constraints)

When they conflict, `KINGDOM.md` wins. `kingdom doctor` scans for the conflict and prompts the user to merge duplicated constraints.

**Implicit context retention mechanism:**

- `KINGDOM.md`: users write long-lived constraints here, and the new manager loads them automatically
- `workspace.note(constraint)`: the manager captures implicit user constraints during conversation and Kingdom persists them

```
User: "This feature needs to support Safari 14"
Manager call: workspace.note("Support Safari 14, do not use new CSS features")
```

**Additional manager tools:**

```
workspace.note(constraint, scope?)   // record a constraint; optional scope: global / directory path / job_id
workspace.notes()                    // view all recorded constraints
```

**Note priority:** narrower scope wins (job > directory > global)

**Conflict detection:** Kingdom does not perform automatic conflict detection. Every time a manager takes over, it reads all notes (`workspace.notes()`) and determines conflicts itself, then uses `workspace.note()` to clean them up.

---

## During Manager Disconnect

**Kingdom responsibilities:**
- Write `job.complete()` immediately into `.kingdom/` without waiting for the manager
- Queue and buffer all worker events
- After recovery, proactively push a summary: completed jobs, paused jobs, and questions awaiting confirmation

**Worker behavior:**
- For `job.request()` with `blocking=true`: pause and wait, do not continue executing
- For `job.request()` with `blocking=false`: continue working, and the question goes into the pending-confirmation queue
- On normal completion: call `job.complete()`, Kingdom stores it, and the manager reviews it after recovery

---

## Cancel Switch

When the user chooses cancel:
- Job is marked `paused`
- The pane shows `[Kingdom] Task paused, waiting for manual handling`
- The user can manually trigger a switch later

---

## Manual Swap (`kingdom swap`)

The user can initiate a switch manually, using the same failover switchover flow.

**Command forms:**

```
kingdom swap worker-1              # show a selectable list of available providers
kingdom swap worker-1 claude       # directly specify the target provider
```

**Flow:**

```
1. Kingdom sends a checkpoint request to the current worker (urgency="high")
2. Wait up to 10 seconds
   → worker submits a checkpoint within 10 seconds → use the checkpoint for handoff
   → timeout → generate a degraded checkpoint from git diff and force the switch
3. Show a confirmation dialog (same as the failover popup, with the reason changed to "manual switch by user")
4. User confirms → follow the standard failover switch flow
```

The only difference from automatic failover is that the reason is marked as `manual`, so it does not count toward the circuit breaker.