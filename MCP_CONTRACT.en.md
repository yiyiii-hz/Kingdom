# Kingdom v2 Design: MCP Contract

## Design Principles

- Manager and worker receive different toolsets
- Default soft limit: workers start with the minimum required permissions
- Manager can dynamically grant workers additional permissions
- All tool calls are recorded in action history for auditing

---

## Job Completion Display

Three-way sync on completion:

```
Worker pane     Show completion summary + "waiting for manager review..."
Status bar      [Codex:impl✓] mark as pending
Manager pane    Kingdom pushes a notification with the result summary and changed files
```

Manager calls `job.result(job_id)` to retrieve the full result and decide the next step.
The worker pane does not close automatically, so the user can keep reviewing the work process.

---

## Manager Toolset

Manager has global control:

```
# Workspace
workspace.status()                          // global state snapshot
workspace.log()                             // action history

# Worker lifecycle
worker.create(provider, role?)              // create worker
worker.assign(worker_id, job_id)            // assign job
worker.release(worker_id)                   // release worker (only callable on idle workers)
                                           // terminate the process immediately, no checkpoint required
                                           // returns an error if the worker has a running job; call job.cancel first
worker.swap(worker_id, new_provider)        // manually switch provider

# Constraint management
workspace.note(constraint)                  // record implicit constraints (captured by manager in conversation)
workspace.notes()                           // view all recorded constraints

# Job management
job.create(intent, worker_id?, depends_on?)  // create a job, optional dependencies
                                           // worker_id is optional: if provided, auto-assign; if omitted, the job stays pending
job.status(job_id)                          // view job status (lightweight: status enum + basic info)
job.result(job_id)                          // get the full result (meaningful only after completion)
                                           // returns: full result_summary, changed_files,
                                           //       checkpoint history, branch name
job.respond(request_id, answer)             // answer a worker request (triggers a Kingdom push to the worker)
job.cancel(job_id)                          // cancel job (two-phase shutdown)
                                           // Phase 1: send graceful stop, wait 30 seconds
                                           // Phase 2a: clean stop, git stash changes
                                           // Phase 2b: after 30-second timeout, force kill, git stash best effort

# Permission management
worker.grant(worker_id, permission)         // grant extra permission to worker
worker.revoke(worker_id, permission)        // revoke permission

# Failover
failover.confirm(worker_id, new_provider)   // confirm switch
failover.cancel(worker_id)                  // cancel switch, pause job
```

---

## Worker Default Toolset (Minimum Permissions)

```
# Task reporting
job.progress(job_id, note)                  // report progress (avoid timeout)
job.complete(job_id, result_summary)        // mark as complete
                                           // result_summary follows a light convention (not strictly structured):
                                           //   what was completed (specific files/features)
                                           //   which files changed
                                           //   remaining issues (optional)
                                           // Kingdom automatically appends the changed_files list
                                           // Basic validation: non-empty, at least 20 Chinese characters
job.fail(job_id, reason)                    // mark as failed
job.cancelled()                             // confirm graceful stop is complete
job.checkpoint(job_id, summary)             // submit a checkpoint (includes a five-part template)
job.request(job_id, question, blocking)     // request guidance from manager
                                           // blocking=true: pause and wait for manager to resume
                                           // blocking=false: keep working; the question queues for manager to handle later
job.request_status(request_id)              // check whether the request has been answered yet (polling fallback mode)

# Read-only status
job.status(job_id)                          // view own job status

# On-demand workspace reads (requested by worker, cached and accelerated by Kingdom)
file.read(path, lines?, symbol?)            // read file contents
                                           // default: first 200 lines + file structure summary
                                           // lines="100-300": read a specific line range
                                           // symbol="LoginForm": read a specific function/class (AST parsing)
workspace.tree(path?)                       // read directory structure
git.log(n?)                                 // read the most recent n commits
git.diff(path?)                             // read the current diff
```

## Worker Initial Context

When a worker starts, Kingdom must provide:
- the job description (the user's original intent)
- the checkpoint / handoff brief (when taking over a task)
- the `changed_files` list (already modified files; do not redo them)

Kingdom preloads in the background and returns immediately when the worker requests them:
- README.md
- files related to the job description (Kingdom guesses based on keywords)

The worker requests any other files as needed, rather than receiving everything upfront.

---

## Worker-Requestable Extended Permissions

Manager can grant these as needed:

| Permission | Description | Typical Scenario |
|---|---|---|
| `subtask.create` | Create new jobs on behalf of the manager (a subtask is a job, with the creator being that worker) | Advanced workers need to split work up |
| `worker.notify` | Notify another worker (forwarded through Kingdom) | Pipeline triggers the next step |
| `workspace.read` | Read global state | A checker needs to see overall progress |
| `job.read_all` | Read all jobs, not just its own | A coordination worker needs to understand overall progress |

---

## Manager Conversation Model

The manager is a continuously running AI process, and the conversation UI is its workspace.

- Users speak directly in the manager pane, and the manager responds in real time
- Kingdom notifications are injected as ordinary messages into the manager conversation stream, and the manager automatically decides the next step after seeing them
- Users can interrupt at any time, adjust direction, and the manager treats user input and Kingdom events as the same conversation context

**Notification format (messages injected into the manager conversation):**

```
[Kingdom] job_001 completed
  worker: Codex
  summary: Implemented login validation, modified 3 files
  changed: src/auth/LoginForm.tsx, src/auth/validation.ts, src/auth/submit.ts
  → call job.result("job_001") to view the full result
```

---

## Kingdom → Manager Push Events

Kingdom only pushes events that require manager intervention; everything else is queried on demand by the manager.

| Event | Trigger Condition |
|---|---|
| `job.completed` | worker calls `job.complete()` |
| `job.failed` | worker calls `job.fail()` or failover is triggered |
| `job.request` | worker calls `job.request()` |
| `job.unblocked` | all dependencies of a job are complete, and the job becomes `pending` |
| `failover.ready` | Kingdom is ready to switch and is waiting for manager confirmation |
| `worker.idle` | worker enters idle state after finishing a task |

During manager disconnects, events are queued and cached; after reconnect, Kingdom pushes a summary.

---

## Communication Flow

```
User
 │
 ▼
Manager (global view)
 │  job.assign / worker.grant
 ▼
Kingdom (MCP server, performs permission arbitration)
 │  distributes the corresponding toolset
 ▼
Worker (restricted view)
 │  job.complete / job.progress
 ▼
Kingdom (records action history, notifies manager)
 │
 ▼
Manager (decides next step)
```

---

## Full Loop for a Worker Requesting Manager Guidance

```
1. Worker calls job.request(job_id, question, blocking=true)
2. Kingdom assigns request_id and notifies the manager (push event job.request)
3. Manager receives the notification and calls job.respond(request_id, answer)
4. Kingdom stores the answer and notifies the worker:
   → Push-supported: MCP server→client notification, worker is awakened
   → Push-unsupported: worker polls job.request_status(request_id) every 10 seconds
5. Worker receives the answer and continues working
```

**When blocking=false:** the worker keeps working without waiting for the answer; Kingdom adds the question to a pending queue, the manager handles it in bulk after returning, and the answer is stored for later reference by the worker.

**Kingdom tells the worker the push mode at startup:** the initial context includes `notification_mode: push | poll`, and the worker chooses its waiting strategy accordingly.

---

## Flow for a Worker Requesting Elevated Permissions

```
1. Worker calls job.request(job_id, "Need to create a subtask")
2. Kingdom notifies the manager
3. Manager decides whether to grant it and calls worker.grant(worker_id, "subtask.create")
4. Kingdom updates that worker's toolset
5. Worker can call subtask.create
```

Permissions are temporary and are automatically revoked when the job completes.

## Full `subtask.create` Flow

```
subtask.create(intent, depends_on?)
```

- A subtask is essentially a job, and the creator is recorded as that worker rather than the manager
- `depends_on=[creator's job_id]` is automatically appended to prevent orphaned jobs
- Kingdom notifies the manager: `[Kingdom] worker-1 created subtask job_003: {intent}`
- The manager decides whether to assign a worker to execute it, exactly like a normal job
- The worker cannot assign a worker to its own subtask (no `worker.assign` permission)