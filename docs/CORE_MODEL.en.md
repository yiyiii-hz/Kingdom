# Kingdom v2 Design: Core Model

> Chinese version: [CORE_MODEL.zh.md](./CORE_MODEL.zh.md)

## Three Core Concepts

### 1. Job

The carrier of user intent. A Job has:

- `intent`: the user’s original description
- `status`: pending / running / done / failed
- `history`: the execution record of every attempt (who did it, what they did, and the result)
- `handoff_summary`: the latest handoff brief (the content passed to the new provider during a switch)

A Job is stable. If the provider changes, the Job does not.

**Job lifecycle:**

- `waiting`: has unresolved dependencies, waiting
- `cancelling`: being cancelled (waiting for the worker to stop gracefully)

**Worker lifecycle:**

```
running → completed → idle → automatic termination after timeout
```

The default idle timeout is 10 minutes, configurable in `.kingdom/config.toml`. After the timeout, Kingdom terminates the worker process.

When the Manager needs a new worker, it must always go through `worker.create()`, getting a new process and new context, rather than reusing an idle worker. Reason: reusing old context is high risk (context pollution + early limit exhaustion), while cold start cost is extremely low (seconds).
- `pending`: dependencies are satisfied, waiting for manager assignment
- `running`: worker is executing
- `completed`: finished
- `failed`: failed
- `paused`: paused by the user or circuit breaker mechanism

**Job dependencies:**

```
job.create("Write frontend integration", depends_on=["job_A"])
```

- When dependencies are not yet complete, the job status is `waiting`
- After dependencies complete, Kingdom notifies the manager, and the job becomes `pending`; the manager decides whether to start it
- If a dependency fails, Kingdom notifies the manager, and the job remains `waiting`; the manager decides what happens next:
  - `job.cancel(job_id)` — cancel
  - `job.keep_waiting(job_id)` — keep waiting until the dependency is fixed
  - `job.update(job_id, new_intent)` — modify the description and start immediately

**Cascading cancellation handling:**

When cancelling `job_A`, Kingdom checks whether any jobs depend on it. If so, it notifies the manager:

```
[Kingdom] Cancelling job_001 will affect the following jobs:
  job_002  Write frontend integration (waiting, depends on job_001)
  job_003  Add unit tests (waiting, depends on job_001)

Please choose: [Cancel all]  [Keep waiting]  [Decide individually]
```

By default, cascading is not automatic; the manager decides. This is the same handling pattern used for dependency failures.

### 2. Provider

The AI that performs the work. Three defaults:

| Provider | Default Strength | Default Model |
|---|---|---|
| Claude | planning, reasoning, coordination | claude-sonnet-4-5 |
| Codex | coding implementation, refactoring, testing | gpt-4o |
| Gemini | frontend, UI, copywriting | gemini-2.0-flash |

**Specify a model per job:**

```
job.create("Design the entire auth architecture", model="claude-opus")    // use a stronger model for complex tasks
job.create("Write a simple button", model="gemini-flash")                 // use a cheaper model for simple tasks
```

Kingdom includes cost-aware prompts by default: when task complexity is low, it recommends switching to a cheaper model.

The default model can be overridden globally in `.kingdom/config.toml`.

These are **default recommendations**, not hard bindings. Any provider can play any role.

### 3. Session

The runtime context of the current workspace. It includes:

- who the current manager is
- which workers are currently running (one per pane)
- which job each worker is handling
- action history (auditable)

---

## Git Strategy

Kingdom does not enforce git behavior; it provides configurable default strategies.

**Default: one branch per job**

```
job starts → git checkout -b kingdom/job_001
worker works on an isolated branch
job completes → Kingdom notifies the manager, branch awaits review
manager decides: merge / continue editing / discard
```

Parallel workers each work on their own branches and do not interfere with each other. Merge conflicts are handled during manager review.

**During failover:** the new provider continues on the same branch, without creating a new one, so history remains continuous.

**Commit timing:**
- `job.checkpoint` → Kingdom automatically commits, commit message: `[kingdom checkpoint] job_001: {checkpoint summary}`
  - each checkpoint has a clean, isolated diff and does not accumulate
- `job.complete` → no automatic commit; the manager decides: merge / squash merge / discard branch

**When `strategy = "commit"`:** both checkpoint and complete are automatically committed to the current branch, with no isolated branch.
**When `strategy = "none"`:** Kingdom does not touch git at all; checkpoints store only text summaries, with no diff snapshot.

**Configurable strategy (`.kingdom/config.toml`):**

```toml
[git]
strategy = "branch"      # branch (default) / commit / none
branch_prefix = "kingdom/"
auto_commit = false       # whether to auto-commit when a job completes
```

- `branch`: one branch per job (recommended, safe for parallel work)
- `commit`: work on the current branch, and auto-commit when the job completes
- `none`: Kingdom does not touch git

---

## Role Handling

v2 does not expose roles to users.

Users only see: **Job** and **Provider**.

`role` (manager / worker) is an internal Kingdom concept that determines which set of MCP tool permissions the provider receives.

---

## Completion Signals

Workers use MCP tool calls to tell Kingdom that a task is complete:

```
job.complete(job_id, result_summary)
job.fail(job_id, failure_reason)
job.progress(job_id, progress_note)  // optional, used for long-running tasks
```

Kingdom does not rely on filesystem artifacts anymore (no longer needs `done.json`).

---

## Failure Classification

| Type | Description | Auto-detected |
|---|---|---|
| `network` | network interruption | ✓ |
| `context_limit` | token limit exceeded / API error | ✓ |
| `process_exit` | provider process exited | ✓ |
| `timeout` | no response within the preset time | ✓ |
| `rate_limit` | API rate limit exceeded (429) | ✓ |
| `manual` | user-triggered replacement | — |
