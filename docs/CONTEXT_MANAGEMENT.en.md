# Kingdom v2 Design: Context Management

> Chinese version: [CONTEXT_MANAGEMENT.zh.md](./CONTEXT_MANAGEMENT.zh.md)

## Goal

Within the provider context limit, give the provider the minimum necessary information to complete the current task.

---

## Context Strategy Differences Between Manager and Worker

| | Worker | Manager |
|---|---|---|
| Context content | Task-oriented, discardable after completion | Relationship-oriented, remembers preferences and decisions |
| Overflow handling | Checkpoint compression + continue | Direct failover, no summarization |
| Basis for a new instance to take over | Checkpoint summary + git diff | Kingdom structured state (`job` / `notes` / `log`) |
| Reason | A summary is enough to continue the task | The real "memory" lives in Kingdom; the conversation history is just the process |

The Manager does not implement a checkpoint mechanism. When the context overflow threshold is reached, it goes directly into the Manager failover flow.

---

## Two-Layer Strategy

### Layer 1: Worker Autonomous Checkpointing

The worker performs periodic summaries on its own, without relying on an external LLM, so quality is highest.

**Trigger timings (graded notifications):**

```
50%  →  urgency="normal"    can be delayed by at most 60 seconds
70%  →  urgency="high"      can be delayed by at most 15 seconds
85%  →  urgency="critical"  no delay allowed, must be done immediately
90%  →  trigger failover, using the most recent checkpoint as the handoff brief
```

**Checkpoint flow:**

1. Kingdom sends: `context.checkpoint_request(job_id, urgency)`
2. Worker responds:
   - Do it immediately: `job.checkpoint(job_id, summary)`
   - Request a delay: `context.checkpoint_defer(job_id, reason, eta_seconds)` (only normal/high are allowed)
3. Kingdom stores the checkpoint + automatically appends the git diff
4. Worker trims old conversation history, and the context drops back to around 15%

**Forced downgrade:** If the checkpoint still has not been created after the delay window, Kingdom generates a downgraded checkpoint using the git diff only (no text summary)

**Mandatory checkpoint template (the worker must answer all five items + Kingdom automatically appends the git snapshot):**

```
1. What was done     Completed work, specific to files/functions
2. What was dropped  Key decisions and why they were made (most important; prevents the next provider from repeating the same mistakes)
3. What is in progress  Work currently underway, precise down to the step
4. What remains      Remaining checklist
5. What pitfalls were hit  Known issues for the next provider to avoid

--- Kingdom automatically appends ---
git_diff:         Full diff of all current uncommitted changes
changed_files:    List of newly added / modified / deleted files
```

**Initial context for the new provider:**

```
The following files were modified by the previous provider; do not reimplement them:
  - src/auth/LoginForm.tsx (validation logic completed)
  - src/auth/validation.ts (completed, do not modify)

Current diff: [git diff content]

Continue from here: [checkpoint item 3 content]
```

**Quality assurance (two layers):**

- **Kingdom basic validation**: all five items must be non-empty, and each must be at least 20 characters; if not, the worker must fill them in again
- **Manager review**: only when failover is triggered, a popup shows the checkpoint content for manager confirmation
  - Satisfied → confirm the switch
  - Not satisfied → ask the worker to add more detail (if the worker is still alive) or switch using the existing content directly (if the worker has already crashed)

**Token savings effect:**

| | No checkpoint | With checkpoint |
|---|---|---|
| Normal work | Context grows linearly until it crashes | Periodically falls back, stays low |
| Restart after failure | New provider starts from zero | Takes over from checkpoint, saving 60-80% |
| Total cost | High (expensive to redo after crashes) | Low (each checkpoint costs about 500-1000 tokens, cuts 20k-30k) |

### Layer 2: Structured Task Handoff

When the Manager assigns a task to a worker, it does not pass along the full conversation history.

It only passes:
- The task description (user intent)
- The necessary code/file context (Kingdom reads it on demand, not all at once)
- Relevant prior decisions (from the handoff summary, not the raw conversation)

---

## Handoff Brief During Switching

When a provider needs to be replaced, Kingdom generates a handoff brief for the new provider:

```
[Kingdom Handoff Brief]
Previous provider: Codex (reason: context overflow)
Job: {job intent}

Completed:
  - Implemented email/password validation for the login form
  - Added inline error prompts

In progress (halfway through):
  - Form submission handling (starts at line 45 in src/auth/submit.ts)

Remaining:
  - UI styling for error states
  - Unit tests

⚠ Possibly incomplete files (being written at the time of crash, check first):
  - src/auth/submit.ts

Relevant files:
  - src/auth/LoginForm.tsx
  - src/auth/validation.ts
```

**Files being written at the time of the crash:** Kingdom compares the last `job.progress` before the crash with the git diff to determine which files were being written at the moment of failure. These are marked separately in the handoff brief and are not rolled back automatically; the new provider decides whether to continue or rewrite them.

The new provider starts from this brief and does not need, and cannot access, the original conversation history.

---

## Token Usage Tracking

### Tracking Strategy

**Primary source: mandatory provider reporting**
- `context.ping` is a required protocol for connecting to Kingdom; if it is not implemented, the provider does not start
- Report once every 30 seconds or after every tool call response
- Data sources for each provider:
  - Claude: `usage.input_tokens`
  - Codex: OpenAI usage fields
  - Gemini: `usageMetadata.totalTokenCount`

**Fallback: Kingdom estimates on its own**
- Enabled when no `context.ping` is received before timeout
- Kingdom intercepts MCP messages and accumulates estimates (not precise, but better than nothing)
- Thresholds drop from 70% to 50% (more conservative)
- Status bar label: `⚠ token tracking degraded`

### Provider Limits

| Provider | Context limit | Compression threshold trigger (70%) |
|---|---|---|
| Claude | 200k tokens | 140k |
| Codex (GPT-4o) | 128k tokens | 90k |
| Gemini | 1M tokens | 700k |

Codex is the highest risk and should be monitored first for long-running tasks.

### Design Principle

It is better to compress too early than to wait until the API errors out. An estimation error of 10-20% is fine; the 70% threshold leaves enough headroom.

---

## Expected Token Savings

| Scenario | Without Kingdom | With Kingdom |
|---|---|---|
| Long-running task (2 hours) | Blows up, manual rebuild needed | Auto-compresses and keeps running |
| Multiple workers in parallel | Each one grows independently | Each one compresses independently |
| Provider switching | New provider starts from zero | New provider gets a compressed brief, saving 60-80% of tokens |
