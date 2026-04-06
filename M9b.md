# M9b：运行时交互 CLI

## 前置条件

M1–M8 + M9a 全部完成。

本文件逐命令追加，当前只含已写完的部分。

---

## 1. `kingdom restart`（`src/cli/restart.rs`）

### 目标

重启 daemon 进程，不中断 tmux session 和 provider 进程。

### 函数签名

```rust
pub async fn run_restart(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>>;
```

### 实现步骤

```
1. 读取 .kingdom/daemon.pid → 解析 PID
   - 文件不存在 → 输出 "Kingdom daemon 未运行" 并正常退出
   - PID 不存活（kill(pid, 0) 失败）→ 输出 "daemon 进程已退出（残留 pid 文件），正在清理..." 并删除 pid 文件，然后直接跳到步骤 4

2. 发送 SIGTERM，输出 "正在停止 daemon (PID {pid})..."

3. 等待最多 5 秒（每 200ms 检查一次 kill(pid, 0)）：
   - 若 5 秒内进程退出 → 继续
   - 若 5 秒后仍存活 → 发送 SIGKILL，输出 "daemon 未响应，强制终止"

4. 重启：通过 watchdog 触发（watchdog 检测 daemon 退出后自动重启）
   - 读取 .kingdom/watchdog.pid，检查 watchdog 是否存活
   - watchdog 存活 → 什么都不做（watchdog 会自动重启 daemon），输出 "等待 watchdog 重启 daemon..."
   - watchdog 不存活 → 直接 spawn daemon 进程（同 `kingdom up` 的启动逻辑）：
       std::process::Command::new(&watchdog_binary)
           .arg(&workspace)
           .stdin(Stdio::null())
           .stdout(Stdio::null())
           .stderr(Stdio::null())
           .spawn()?;

5. 等待 daemon 重启（最多 15 秒，每 500ms 检查 .kingdom/daemon.pid 是否出现新 PID）：
   - 若检测到新 PID 且进程存活 → 输出 "✓ daemon 已重启，PID {new_pid}"
   - 超时 → 输出 "⚠ daemon 未能在 15 秒内重启，请检查日志" 并以非零退出

6. 从 state.json 读取 worker 列表，输出各 provider 进程是否仍存活：
   - 对每个 worker.pid → kill(pid, 0)
   - 存活：✓ {worker_id} ({provider})  PID {pid}  进程存活
   - 不存活：⚠ {worker_id} ({provider})  进程已退出（需手动处理）
```

### PID 文件路径

- daemon：`{storage.root}/daemon.pid`（即 `.kingdom/daemon.pid`）
- watchdog：`{storage.root}/watchdog.pid`
- watchdog binary：`current_exe().parent()/"kingdom-watchdog"`（同 `up.rs`）

### 输出示例

```
正在停止 daemon (PID 12345)...
daemon 已停止，等待 watchdog 重启 daemon...
✓ daemon 已重启，PID 12401

Provider 进程状态：
  ✓ w1 (claude)   PID 11200  进程存活
  ✓ w2 (codex)    PID 11210  进程存活
```

### 注册

`src/cli/mod.rs` 新增 `pub mod restart;`

`src/bin/kingdom.rs` 新增：

```rust
Restart {
    #[arg(default_value = ".")]
    workspace: PathBuf,
},
```

main() 中：
```rust
Commands::Restart { workspace } => {
    kingdom_v2::cli::restart::run_restart(workspace)
        .await
        .unwrap_or_else(|e| { eprintln!("Error: {e}"); std::process::exit(1); });
}
```

### 单元测试

**不需要**集成测试（涉及真实进程信号）。只需：

- `test_restart_missing_pid_file` —— pid 文件不存在时函数返回 Ok 并输出提示（用 tempdir 模拟 storage.root）
- `test_restart_stale_pid_file` —— pid 文件存在但写入一个不存在的 PID（如 99999999），函数清理 pid 文件后继续（不 panic）

测试中用 `tempfile::tempdir()` 创建 workspace，直接写假 pid 文件。

### 验收条件

- [ ] daemon 未运行时正常退出，输出友好提示
- [ ] pid 文件存在但进程已退出时清理 pid 文件，不 panic
- [ ] SIGTERM → 等待 5s → SIGKILL 流程正确（可通过 down.rs 的 terminate_by_pid_file 借鉴）
- [ ] watchdog 存活时不重复 spawn daemon
- [ ] 等待 15 秒超时后非零退出
- [ ] provider 进程 PID 状态正确输出

---

## 2. `kingdom swap` 补全（`src/cli/swap.rs`）

### 目标

当用户不传 provider 时，从 session 的 `available_providers` 列表中交互式选择，而不是直接透传 `None`（当前行为是把 `None` 传给 `queue_manual_swap`，让 recommender 自动选）。

### 现状

`run_swap` 已存在，`provider: None` 时直接透传给 `queue_manual_swap`（recommender 自动推荐）。现在要在透传之前加一个 **interactive 路径**。

### 修改点：仅修改 `run_swap`

```rust
pub async fn run_swap(
    workspace: PathBuf,
    worker_id: String,
    provider: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Arc::new(Storage::init(&workspace)?);

    // 若未指定 provider，从 session.available_providers 列表中让用户选择
    let provider = match provider {
        Some(p) => Some(p),
        None => prompt_provider_selection(&storage, &worker_id)?,
    };

    if try_swap_via_daemon(&workspace, &worker_id, provider.clone()).await? {
        println!("queued manual swap for worker");
        return Ok(());
    }
    queue_manual_swap(&storage, &workspace, &worker_id, provider, None, None).await?;
    println!("queued manual swap for worker");
    Ok(())
}
```

### 新增函数 `prompt_provider_selection`

```rust
fn prompt_provider_selection(
    storage: &Storage,
    worker_id: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    use std::io::Write;

    let session = match storage.load_session()? {
        Some(s) => s,
        None => return Ok(None),   // 无 session，让 recommender 自动选
    };

    // 过滤掉当前 worker 的 provider（不显示自己）
    let current_provider = session
        .workers
        .get(worker_id)
        .map(|w| w.provider.as_str())
        .unwrap_or("");

    let candidates: Vec<&str> = session
        .available_providers
        .iter()
        .map(|p| p.as_str())
        .filter(|p| *p != current_provider)
        .collect();

    if candidates.is_empty() {
        println!("没有其他可用 provider，将由系统自动推荐。");
        return Ok(None);
    }

    println!("选择目标 provider（当前：{current_provider}）：");
    for (i, p) in candidates.iter().enumerate() {
        println!("  {}) {}", i + 1, p);
    }
    print!("输入编号 [1]: ");
    std::io::stdout().flush()?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let idx = line.trim().parse::<usize>().unwrap_or(1).saturating_sub(1);
    let chosen = candidates.get(idx).copied().unwrap_or(candidates[0]);
    Ok(Some(chosen.to_string()))
}
```

### 边界条件

- `available_providers` 为空或只剩当前 provider：输出提示，返回 `None`（fallback 到 recommender）
- 用户输入非数字 / 越界：默认选第 1 个（`unwrap_or(1)`）
- session 不存在（daemon 未运行）：返回 `None`，让后续流程报错或 fallback

### 单元测试

`prompt_provider_selection` 有 stdin 依赖，不适合单元测试。只需：

- `test_swap_no_session_returns_none` —— storage 无 session 时 `prompt_provider_selection` 返回 `Ok(None)` 不 panic
- `test_swap_provider_list_filters_current` —— session 中 w1 provider=codex，available_providers=[codex,claude]，candidates 只含 claude

### 验收条件

- [ ] `kingdom swap w1`（不传 provider）显示可用 provider 列表，过滤掉当前 provider
- [ ] 列表为空时输出提示并 fallback 到 recommender（不 panic）
- [ ] `kingdom swap w1 claude`（传 provider）行为不变，不触发交互

---

## 3. `kingdom replay`（`src/cli/replay.rs` + `src/mcp/cli_server.rs`）

### 目标

读取已有 job 的 intent，用相同 intent 创建一个新 job，可选择立即分配给空闲 worker。

### 两部分改动

#### 3a. cli_server.rs：添加 "replay" 命令

在 `handle_command` 中新增分支（紧接 `"swap"` 之后）：

```rust
Some("replay") => {
    let job_id = match request.get("job_id").and_then(Value::as_str) {
        Some(id) => id,
        None => return json!({"ok": false, "error": "missing job_id"}),
    };
    let assign = request.get("assign").and_then(Value::as_bool).unwrap_or(false);

    let session = match storage.load_session() {
        Ok(Some(s)) => s,
        _ => return json!({"ok": false, "error": "no active session"}),
    };

    let original = match session.jobs.get(job_id) {
        Some(j) => j.clone(),
        None => return json!({"ok": false, "error": format!("job not found: {job_id}")}),
    };

    // 创建新 job
    let new_job_id = format!("job_{:03}", session.job_seq + 1);
    let new_job = crate::types::Job {
        id: new_job_id.clone(),
        intent: original.intent.clone(),
        status: crate::types::JobStatus::Pending,
        worker_id: None,
        depends_on: vec![],
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        branch: None,
        branch_start_commit: None,
        checkpoints: vec![],
        result: None,
        fail_count: 0,
        last_fail_at: None,
    };

    let mut session = session;
    session.jobs.insert(new_job_id.clone(), new_job);
    session.job_seq += 1;

    // 若 assign=true 且有空闲 worker，自动分配
    let assigned_worker = if assign {
        let idle_worker = session.workers.values()
            .find(|w| {
                w.role == crate::types::WorkerRole::Worker
                    && w.status == crate::types::WorkerStatus::Idle
                    && w.job_id.is_none()
            })
            .map(|w| w.id.clone());

        if let Some(wid) = idle_worker {
            if let Some(w) = session.workers.get_mut(&wid) {
                w.job_id = Some(new_job_id.clone());
                w.status = crate::types::WorkerStatus::Running;
            }
            if let Some(j) = session.jobs.get_mut(&new_job_id) {
                j.worker_id = Some(wid.clone());
                j.status = crate::types::JobStatus::Running;
            }
            Some(wid)
        } else {
            None
        }
    } else {
        None
    };

    if let Err(e) = storage.save_session(&session) {
        return json!({"ok": false, "error": e.to_string()});
    }

    json!({
        "ok": true,
        "data": {
            "new_job_id": new_job_id,
            "intent": original.intent,
            "assigned_worker": assigned_worker,
        }
    })
}
```

#### 3b. src/cli/replay.rs：新建

```rust
pub async fn run_replay(
    workspace: PathBuf,
    job_id: String,
) -> Result<(), Box<dyn std::error::Error>>;
```

实现流程：

```
1. 连接 CLI server（同 try_swap_via_daemon 的连接模式）
   - socket: /tmp/kingdom/{hash}-cli.sock
   - 连接失败 → 输出 "Kingdom daemon 未运行，无法 replay（需要 daemon 创建新 job）" 并退出非零

2. 先发送 {"cmd":"replay","job_id":"{job_id}","assign":false}
   - 获取 new_job_id 和 intent
   - 若 job not found → 输出错误退出

3. 读取 session 检查是否有空闲 worker
   - 连接 CLI server 发送 {"cmd":"ready"} 确认 daemon 存活（已连接则跳过，直接读 state.json）
   - 实际上从 state.json 读取（不需要再走 CLI server）

4. 若有空闲 worker，询问用户：
   立刻分配给 {worker_id}（{provider}）？[Y/n]

5. 用户选 Y（或直接回车）→ 重发 {"cmd":"replay","job_id":"{job_id}","assign":true}
   用户选 n → 不重发

6. 输出结果：
   ✓ 重新创建 job_004，intent：{intent[:60]}
   已分配给 w2 (codex)   ← 仅当 assign 时
```

**注意：** 步骤 2 先发 `assign:false` 创建 job，步骤 5 如果用户选 Y，需要重新考虑逻辑——更简洁的实现是：

- 不先发 `assign:false`，而是先从 **state.json** 读取 job 的 intent 和空闲 worker 列表（本地读，不走 CLI server）
- 询问用户后，再发一次 `{"cmd":"replay","job_id":..., "assign": true/false}`（只发一次）

采用这个更简洁的实现：

```rust
pub async fn run_replay(
    workspace: PathBuf,
    job_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;

    // 本地读 session，获取 intent 和空闲 worker
    let session = storage.load_session()?
        .ok_or("no active session; run `kingdom up` first")?;

    let original = session.jobs.get(&job_id)
        .ok_or_else(|| format!("job not found: {job_id}"))?;

    let intent = original.intent.clone();

    let idle_worker = session.workers.values()
        .find(|w| {
            w.role == crate::types::WorkerRole::Worker
                && w.status == crate::types::WorkerStatus::Idle
                && w.job_id.is_none()
        })
        .cloned();

    // 询问是否立即分配
    let assign = if let Some(ref w) = idle_worker {
        print!("立刻分配给 {}（{}）？[Y/n] ", w.id, w.provider);
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let ans = line.trim().to_ascii_lowercase();
        ans.is_empty() || ans == "y"
    } else {
        false
    };

    // 连接 CLI server，发送一次 replay 命令
    let hash = crate::config::workspace_hash(&workspace);
    let socket_path = format!("/tmp/kingdom/{hash}-cli.sock");
    let stream = tokio::net::UnixStream::connect(&socket_path).await
        .map_err(|_| "Kingdom daemon 未运行，无法 replay（需要 daemon 创建新 job）")?;

    // ... 发送/接收 JSON（同 try_swap_via_daemon 模式）...

    let response = send_cli_command(&socket_path, serde_json::json!({
        "cmd": "replay",
        "job_id": job_id,
        "assign": assign,
    })).await?;

    let new_job_id = response["data"]["new_job_id"].as_str().unwrap_or("?");
    let truncated = intent.chars().take(60).collect::<String>();
    println!("✓ 重新创建 {new_job_id}，intent：{truncated}");
    if assign {
        if let Some(ref w) = idle_worker {
            println!("已分配给 {} ({})", w.id, w.provider);
        }
    }
    Ok(())
}

// 抽取发送 CLI 命令的辅助函数（可复用于 restart 等）
async fn send_cli_command(
    socket_path: &str,
    request: serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(socket_path).await
        .map_err(|_| "Kingdom daemon 未运行")?;
    let mut reader = BufReader::new(stream);
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    reader.get_mut().write_all(&bytes).await?;
    reader.get_mut().flush().await?;
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let response: serde_json::Value = serde_json::from_str(&line)?;
    if response["ok"].as_bool() != Some(true) {
        let err = response["error"].as_str().unwrap_or("unknown error").to_string();
        return Err(err.into());
    }
    Ok(response)
}
```

`send_cli_command` 可以放在 `src/cli/mod.rs` 或单独的 `src/cli/daemon_client.rs`，供 replay / restart 等复用。

### CLI 注册

`src/cli/mod.rs` 新增 `pub mod replay;`（若 `send_cli_command` 独立提取，同时新增 `pub mod daemon_client;`）

`src/bin/kingdom.rs` 新增：

```rust
Replay {
    #[arg(default_value = ".")]
    workspace: PathBuf,
    job_id: String,
},
```

main() 中异步调用 `run_replay`。

### 单元测试

cli_server.rs 中新增：

- `test_cli_server_replay_creates_new_job` —— 发送 `{"cmd":"replay","job_id":"job_001","assign":false}`，验证响应含 `new_job_id`，session 中 job 数量 +1
- `test_cli_server_replay_assign_attaches_idle_worker` —— session 中有 idle worker，`assign:true` 时新 job status=Running 且 worker_id 已设置
- `test_cli_server_replay_unknown_job` —— 不存在的 job_id 返回 `ok:false`

### 验收条件

- [ ] `kingdom replay job_001` 创建新 job，intent 与原 job 一致
- [ ] 有空闲 worker 时询问是否分配，Y 时新 job status=Running
- [ ] daemon 未运行时输出友好错误，非零退出
- [ ] 不存在的 job_id 返回错误
- [ ] cli_server 的 replay 命令测试通过

---

## 4. `kingdom job diff`（`src/cli/job_diff.rs`）

### 目标

显示某个 job 从开始到完成期间产生的 git diff。纯只读操作，不需要 daemon。

### 函数签名

```rust
pub fn run_job_diff(
    workspace: PathBuf,
    job_id: String,
) -> Result<(), Box<dyn std::error::Error>>;
```

### 实现逻辑

```rust
pub fn run_job_diff(workspace: PathBuf, job_id: String) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;

    let session = storage.load_session()?
        .ok_or("no active session")?;

    let job = session.jobs.get(&job_id)
        .ok_or_else(|| format!("job not found: {job_id}"))?;

    // git strategy = None 时无法 diff
    if matches!(session.git_strategy, crate::types::GitStrategy::None) {
        println!("该 job 在非 git 模式下运行，无 diff 记录。");
        return Ok(());
    }

    let start_commit = match &job.branch_start_commit {
        Some(c) => c.clone(),
        None => {
            println!("job {job_id} 尚未产生 git commit（可能未完成）。");
            return Ok(());
        }
    };

    // 若 job 有 changed_files，只 diff 这些文件；否则 diff 所有变更
    let status = if job.result.as_ref().map(|r| r.changed_files.is_empty()).unwrap_or(true) {
        std::process::Command::new("git")
            .args(["-C", workspace.to_str().unwrap_or("."), "diff", &start_commit, "HEAD"])
            .status()?
    } else {
        let files = job.result.as_ref().unwrap().changed_files.clone();
        std::process::Command::new("git")
            .args(["-C", workspace.to_str().unwrap_or("."), "diff", &start_commit, "HEAD", "--"])
            .args(&files)
            .status()?
    };

    if !status.success() {
        return Err(format!("git diff 失败（exit {}）", status.code().unwrap_or(-1)).into());
    }
    Ok(())
}
```

### 边界条件

| 情形 | 输出 |
|---|---|
| `git_strategy == None` | `"该 job 在非 git 模式下运行，无 diff 记录。"` |
| `branch_start_commit` 为 None | `"job {id} 尚未产生 git commit（可能未完成）。"` |
| job_id 不存在 | Err "job not found: {id}" |
| session 不存在 | Err "no active session" |
| git diff 返回非零 | Err "git diff 失败（exit N）" |

已归档的 job（result 已移至 archive）：`branch_start_commit` 保存在 `state.json` 的 `job` 条目中，不依赖文件系统中的 result.json，因此即使归档后仍可 diff。

### CLI 注册

`src/cli/mod.rs` 新增 `pub mod job_diff;`

`src/bin/kingdom.rs` 新增（作为 `job` 子命令的子命令，或简化为顶层命令 `job-diff`）：

```rust
JobDiff {
    #[arg(default_value = ".")]
    workspace: PathBuf,
    job_id: String,
},
```

clap 命令名用 `#[command(name = "job-diff")]`。main() 中调用 `run_job_diff`（同步函数，无需 `.await`）。

### 单元测试

- `test_job_diff_no_git_strategy` —— `git_strategy = None` 时输出提示并返回 Ok（用 tempdir，不调用 git）
- `test_job_diff_no_start_commit` —— `branch_start_commit = None` 时输出提示并返回 Ok
- `test_job_diff_missing_job` —— job_id 不存在时返回 Err

测试中构造 session 写入 storage，不实际调用 `git diff`（只测边界条件，git 调用路径靠人工验证）。

### 验收条件

- [ ] `git_strategy = None` 时输出友好提示
- [ ] `branch_start_commit = None` 时输出友好提示
- [ ] job_id 不存在时非零退出
- [ ] 有 `changed_files` 时 git diff 命令中包含 `--` 和文件列表
- [ ] 已归档 job（result.json 已移走）仍可正常调用（state.json 中 job 元数据完整）

---

## 5. `kingdom open`（`src/cli/open.rs`）

### 目标

跳转到指定 worker 或 job 对应的 tmux pane。纯只读操作，不需要 daemon。

### 函数签名

```rust
pub fn run_open(
    workspace: PathBuf,
    target: String,   // worker_id（如 "w1"）或 job_id（如 "job_001"）
) -> Result<(), Box<dyn std::error::Error>>;
```

### 实现逻辑

```rust
pub fn run_open(workspace: PathBuf, target: String) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;

    let session = storage.load_session()?
        .ok_or("no active session")?;

    // 解析 target：先尝试作为 worker_id，再尝试作为 job_id
    let pane_id = resolve_pane_id(&session, &target)
        .ok_or_else(|| format!("找不到 worker 或 job：{target}"))?;

    if pane_id.is_empty() {
        println!("pane 已关闭（job 已结束）。");
        return Ok(());
    }

    let status = std::process::Command::new("tmux")
        .args(["select-pane", "-t", &pane_id])
        .status()?;

    if !status.success() {
        println!("pane 已关闭（job 已结束）。");
    }
    Ok(())
}

fn resolve_pane_id(session: &crate::types::Session, target: &str) -> Option<String> {
    // 1. 直接匹配 worker_id
    if let Some(worker) = session.workers.get(target) {
        return Some(worker.pane_id.clone());
    }
    // 2. 匹配 job_id → 找执行该 job 的 worker
    if let Some(job) = session.jobs.get(target) {
        if let Some(worker_id) = &job.worker_id {
            return session.workers.get(worker_id).map(|w| w.pane_id.clone());
        }
        // job 存在但无 worker（已完成/未分配）→ 返回空字符串，调用方输出提示
        return Some(String::new());
    }
    None
}
```

### 边界条件

| 情形 | 输出 |
|---|---|
| target 是有效 worker_id | 调用 `tmux select-pane -t {pane_id}` |
| target 是有效 job_id，job 有 worker_id | 通过 worker 找到 pane_id，调用 select-pane |
| target 是有效 job_id，但 job 无 worker（已完成）| `"pane 已关闭（job 已结束）。"` |
| target 不匹配任何 worker 或 job | Err "找不到 worker 或 job：{target}" |
| tmux select-pane 失败（pane 已关闭）| `"pane 已关闭（job 已结束）。"` 并返回 Ok |
| session 不存在 | Err "no active session" |

### CLI 注册

`src/cli/mod.rs` 新增 `pub mod open;`

`src/bin/kingdom.rs` 新增：

```rust
Open {
    #[arg(default_value = ".")]
    workspace: PathBuf,
    target: String,
},
```

main() 中调用 `run_open`（同步函数）。

### 单元测试

`resolve_pane_id` 是纯函数，直接测：

- `test_open_resolve_by_worker_id` —— session 中有 w1，resolve("w1") 返回 w1.pane_id
- `test_open_resolve_by_job_id` —— job_001.worker_id = "w1"，resolve("job_001") 返回 w1.pane_id
- `test_open_resolve_job_no_worker` —— job 无 worker_id，返回 Some("")
- `test_open_resolve_unknown` —— 未知 target 返回 None

### 验收条件

- [ ] `kingdom open w1` 调用 `tmux select-pane -t {pane_id}`（mock tmux 可通过 PATH 覆盖或手动验证）
- [ ] `kingdom open job_001` 通过 job.worker_id 找到 pane，调用 select-pane
- [ ] 未知 target 非零退出
- [ ] pane 不存在时（tmux 返回非零）输出提示并正常退出（不 panic）

---

## 6. Notification Webhook（`src/notifications/webhook.rs`）

### Cargo.toml 新增依赖

```toml
[dependencies]
reqwest = { version = "0.12", features = ["json"], default-features = false }
```

**注意：** 使用 `default-features = false` + `features = ["json"]`，避免引入 openssl 编译依赖。reqwest 0.12 默认使用 rustls，只需再加 `features = ["rustls-tls"]`：

```toml
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
```

### Config 扩展（`src/config/mod.rs`）

在 `KingdomConfig` 中新增：

```rust
#[serde(default)]
pub webhook: WebhookConfig,
```

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct WebhookConfig {
    pub url: Option<String>,
    #[serde(default = "default_webhook_events")]
    pub events: Vec<String>,
    #[serde(default = "default_webhook_timeout")]
    pub timeout_seconds: u64,
}

fn default_webhook_events() -> Vec<String> {
    vec![
        "job.completed".to_string(),
        "job.failed".to_string(),
        "failover.triggered".to_string(),
    ]
}

fn default_webhook_timeout() -> u64 { 5 }
```

对应 config.toml：

```toml
[webhook]
url = "https://hooks.slack.com/..."
events = ["job.completed", "job.failed", "failover.triggered"]
timeout_seconds = 5
```

### `src/notifications/webhook.rs`

```rust
use crate::config::WebhookConfig;
use crate::storage::Storage;
use crate::types::ManagerNotification;
use std::sync::Arc;

pub struct WebhookNotifier {
    config: WebhookConfig,
    workspace_path: String,
    storage: Arc<Storage>,
    client: reqwest::Client,
}

impl WebhookNotifier {
    pub fn new(config: WebhookConfig, workspace_path: String, storage: Arc<Storage>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_seconds))
            .build()
            .unwrap_or_default();
        Self { config, workspace_path, storage, client }
    }

    /// 将 ManagerNotification 转换为 webhook event name（如 "job.completed"）。
    /// 返回 None 表示该 notification 不触发 webhook。
    pub fn event_name(notification: &ManagerNotification) -> Option<&'static str> {
        match notification {
            ManagerNotification::JobCompleted { .. } => Some("job.completed"),
            ManagerNotification::JobFailed { .. }    => Some("job.failed"),
            ManagerNotification::FailoverReady { .. } => Some("failover.triggered"),
            _ => None,
        }
    }

    /// 检查该 event 是否在订阅列表中。
    fn is_subscribed(&self, event: &str) -> bool {
        self.config.events.iter().any(|e| e == event)
    }

    /// 构造 payload JSON。
    pub fn build_payload(
        notification: &ManagerNotification,
        workspace: &str,
    ) -> serde_json::Value {
        use serde_json::json;
        match notification {
            ManagerNotification::JobCompleted { job_id, worker_id, summary, .. } => json!({
                "event": "job.completed",
                "job_id": job_id,
                "worker": worker_id,
                "summary": summary,
                "workspace": workspace,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ManagerNotification::JobFailed { job_id, worker_id, reason } => json!({
                "event": "job.failed",
                "job_id": job_id,
                "worker": worker_id,
                "reason": reason,
                "workspace": workspace,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ManagerNotification::FailoverReady { worker_id, reason, candidates } => json!({
                "event": "failover.triggered",
                "worker": worker_id,
                "reason": format!("{:?}", reason),
                "candidates": candidates,
                "workspace": workspace,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            _ => serde_json::Value::Null,
        }
    }

    /// 发送 webhook。失败时静默跳过并写 action log 警告。
    pub async fn send(&self, notification: &ManagerNotification) {
        let url = match &self.config.url {
            Some(u) if !u.is_empty() => u.clone(),
            _ => return,   // 未配置 url，直接跳过
        };

        let event = match Self::event_name(notification) {
            Some(e) => e,
            None => return,
        };

        if !self.is_subscribed(event) {
            return;
        }

        let payload = Self::build_payload(notification, &self.workspace_path);
        if payload.is_null() {
            return;
        }

        match self.client.post(&url).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status().as_u16();
                tracing::warn!(url, status, "webhook returned non-2xx, skipping");
                self.log_warning(format!("webhook {url} returned {status}"));
            }
            Err(e) => {
                tracing::warn!(url, error = %e, "webhook failed, skipping");
                self.log_warning(format!("webhook {url} failed: {e}"));
            }
        }
    }

    fn log_warning(&self, message: String) {
        let entry = crate::types::ActionLogEntry {
            timestamp: chrono::Utc::now(),
            actor: "kingdom".to_string(),
            action: "webhook.warning".to_string(),
            params: serde_json::json!({ "message": message }),
            result: None,
            error: Some(message),
        };
        let _ = self.storage.append_action_log(&entry);
    }
}
```

### notifications/mod.rs 更新

新增：`pub mod webhook;`

### 使用点（daemon 中）

`WebhookNotifier::send` 由 daemon 在处理 `ManagerNotification` 时调用（与 `ManagerNotifier` 并排），M10 端到端集成时接线。M9b 只需实现逻辑，无需在 daemon 接线。

### 单元测试

**不需要**真实 HTTP 服务器。只测纯函数：

- `test_webhook_event_name_mapping` —— JobCompleted → "job.completed"，JobFailed → "job.failed"，WorkerIdle → None
- `test_webhook_is_subscribed` —— events=["job.completed"]，"job.failed" 返回 false
- `test_webhook_build_payload_job_completed` —— payload 含 event/job_id/worker/summary/workspace/timestamp
- `test_webhook_url_empty_skips_silently` —— url=None 时 `send` 不 panic（需要 tokio::test，mock 掉 client 或直接测 return 路径）

最后一个测试可以这样写：因为 url 为 None 时 `send` 直接 return，不会构造 reqwest 请求，所以构造一个 `WebhookNotifier { config: WebhookConfig { url: None, .. }, .. }` 并调用 `send`，断言不 panic 即可。

### 验收条件

- [ ] `Cargo.toml` 新增 reqwest，`cargo build` 通过
- [ ] `WebhookConfig` 默认值正确（events 含 3 个，timeout=5）
- [ ] `event_name` 映射正确，不在列表的 notification 返回 None
- [ ] `build_payload` 包含 event / job_id / worker / workspace / timestamp 字段
- [ ] url 为 None 时 `send` 静默跳过，不 panic
- [ ] HTTP 失败（模拟 5xx）时写 action log 警告，不阻塞
- [ ] 事件不在 `events` 订阅列表时跳过（不发请求）

---

## M9b 总验收条件

- [ ] `cargo test` 全部通过（含 M9a 和 M9b 新增测试）
- [ ] `cargo clippy` 无 error
- [ ] `cargo build` 通过（含 reqwest 依赖）
- [ ] bin/kingdom.rs 包含所有新子命令：restart / replay / job-diff / open
- [ ] cli/mod.rs 包含所有新模块：restart / replay / job_diff / open
- [ ] notifications/mod.rs 导出 webhook 模块
