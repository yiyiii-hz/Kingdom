use crate::cli::daemon_client::{send_cli_command, socket_path};
use crate::storage::Storage;
use crate::types::{WorkerRole, WorkerStatus};
use std::io::Write;
use std::path::PathBuf;

pub async fn run_replay(
    workspace: PathBuf,
    job_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;
    let session = storage
        .load_session()?
        .ok_or("no active session; run `kingdom up` first")?;

    let original = session
        .jobs
        .get(&job_id)
        .ok_or_else(|| format!("job not found: {job_id}"))?;
    let intent = original.intent.clone();

    let idle_worker = session
        .workers
        .values()
        .find(|worker| {
            worker.role == WorkerRole::Worker
                && worker.status == WorkerStatus::Idle
                && worker.job_id.is_none()
        })
        .cloned();

    let assign = if let Some(worker) = &idle_worker {
        print!("立刻分配给 {}（{}）？[Y/n] ", worker.id, worker.provider);
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let answer = line.trim().to_ascii_lowercase();
        answer.is_empty() || answer == "y"
    } else {
        false
    };

    let socket = socket_path(&workspace);
    let response = send_cli_command(
        &socket,
        serde_json::json!({
            "cmd": "replay",
            "job_id": job_id,
            "assign": assign,
        }),
    )
    .await
    .map_err(|_| "Kingdom daemon 未运行，无法 replay（需要 daemon 创建新 job）")?;

    let new_job_id = response["data"]["new_job_id"].as_str().unwrap_or("?");
    let truncated = truncate_intent(&intent, 60);
    println!("✓ 重新创建 {new_job_id}，intent：{truncated}");
    if let Some(worker_id) = response["data"]["assigned_worker"].as_str() {
        if let Some(worker) = session.workers.get(worker_id) {
            println!("已分配给 {} ({})", worker.id, worker.provider);
        }
    }

    Ok(())
}

fn truncate_intent(intent: &str, limit: usize) -> String {
    let mut truncated = intent.chars().take(limit).collect::<String>();
    if intent.chars().count() > limit {
        truncated.push_str("...");
    }
    truncated
}
