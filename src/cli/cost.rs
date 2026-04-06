use crate::config::KingdomConfig;
use crate::storage::Storage;
use crate::types::{ActionLogEntry, Session};
use chrono::{Datelike, Local, Utc};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;

pub fn run_cost(workspace: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let storage = Storage::init(&workspace)?;
    let entries = storage.read_action_log(None)?;
    let session = storage.load_session()?;
    let config = KingdomConfig::load_or_default(&storage.root.join("config.toml"));
    let report = build_cost_report(&entries, session.as_ref(), &config.cost, Utc::now());
    print!("{}", render_cost_report(&report));
    Ok(())
}

#[derive(Default)]
struct CostReport {
    today_total: f64,
    week_total: f64,
    month_total: f64,
    providers: Vec<ProviderCost>,
    most_expensive_job: Option<JobCost>,
    has_any_data: bool,
}

#[derive(Default)]
struct ProviderCost {
    provider: String,
    today_tokens: u64,
    today_cost: f64,
}

struct JobCost {
    job_id: String,
    intent: String,
    cost: f64,
}

fn build_cost_report(
    entries: &[ActionLogEntry],
    session: Option<&Session>,
    cost: &crate::config::CostConfig,
    now: chrono::DateTime<Utc>,
) -> CostReport {
    let mut provider_by_worker = HashMap::new();
    let mut job_intents = HashMap::new();
    if let Some(session) = session {
        for worker in session.workers.values() {
            provider_by_worker.insert(worker.id.clone(), worker.provider.clone());
        }
        for job in session.jobs.values() {
            job_intents.insert(job.id.clone(), job.intent.clone());
        }
    }

    for entry in entries {
        if entry.action == "worker.create" {
            if let Some(worker_id) = entry
                .result
                .as_ref()
                .and_then(|v| v.get("worker_id"))
                .and_then(|v| v.as_str())
            {
                if let Some(provider) = entry
                    .params
                    .get("provider")
                    .and_then(|value| value.as_str())
                {
                    provider_by_worker.insert(worker_id.to_string(), provider.to_string());
                }
            }
        }
    }

    let mut last_token_by_worker: HashMap<String, u64> = HashMap::new();
    let mut today_by_provider: HashMap<String, (u64, f64)> = HashMap::new();
    let mut job_costs: HashMap<String, f64> = HashMap::new();
    let mut report = CostReport::default();
    let local_now = now.with_timezone(&Local);

    for entry in entries {
        match entry.action.as_str() {
            "context.ping" => {
                let worker_id = entry.actor.clone();
                let token_count = entry
                    .params
                    .get("token_count")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                let delta = token_count
                    .saturating_sub(last_token_by_worker.get(&worker_id).copied().unwrap_or(0));
                last_token_by_worker.insert(worker_id.clone(), token_count);
                if delta == 0 {
                    continue;
                }
                report.has_any_data = true;
                let provider = provider_by_worker
                    .get(&worker_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                let delta_cost = price_for_provider(cost, &provider, delta);
                let local_ts = entry.timestamp.with_timezone(&Local);
                if same_day(local_ts, local_now) {
                    let provider_entry = today_by_provider
                        .entry(provider.clone())
                        .or_insert((0, 0.0));
                    provider_entry.0 += delta;
                    provider_entry.1 += delta_cost;
                    report.today_total += delta_cost;
                }
                if local_ts.iso_week() == local_now.iso_week() {
                    report.week_total += delta_cost;
                }
                if local_ts.year() == local_now.year() && local_ts.month() == local_now.month() {
                    report.month_total += delta_cost;
                }
                if let Some(job_id) = entry.params.get("job_id").and_then(|value| value.as_str()) {
                    *job_costs.entry(job_id.to_string()).or_default() += delta_cost;
                }
            }
            "compressed_summary" => {
                let tokens = entry
                    .params
                    .get("tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                if tokens == 0 {
                    continue;
                }
                report.has_any_data = true;
                let compressed_cost = price_for_provider(cost, "codex", tokens);
                let local_ts = entry.timestamp.with_timezone(&Local);
                if same_day(local_ts, local_now) {
                    report.today_total += compressed_cost;
                }
                if local_ts.iso_week() == local_now.iso_week() {
                    report.week_total += compressed_cost;
                }
                if local_ts.year() == local_now.year() && local_ts.month() == local_now.month() {
                    report.month_total += compressed_cost;
                }
            }
            _ => {}
        }
    }

    let mut providers = today_by_provider
        .into_iter()
        .map(|(provider, (today_tokens, today_cost))| ProviderCost {
            provider,
            today_tokens,
            today_cost,
        })
        .collect::<Vec<_>>();
    providers.sort_by(|a, b| b.today_cost.total_cmp(&a.today_cost));
    report.providers = providers;

    report.most_expensive_job = job_costs
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(job_id, cost)| JobCost {
            intent: job_intents
                .get(&job_id)
                .cloned()
                .unwrap_or_else(|| "-".to_string()),
            job_id,
            cost,
        });

    report
}

fn render_cost_report(report: &CostReport) -> String {
    if !report.has_any_data {
        return "暂无费用数据（context.ping 尚未写入 action log）\n".to_string();
    }

    let mut output = String::new();
    let _ = writeln!(output, "今日花费：${:.2}", report.today_total);
    let total = report
        .providers
        .iter()
        .map(|item| item.today_cost)
        .sum::<f64>();
    for provider in &report.providers {
        let _ = writeln!(
            output,
            "  {:<8} {:>5}k tokens   ${:<4.2}  {}",
            capitalize(&provider.provider),
            provider.today_tokens / 1000,
            provider.today_cost,
            progress_bar(provider.today_cost, total)
        );
    }
    let _ = writeln!(
        output,
        "\n本周：${:.2}  本月：${:.2}",
        report.week_total, report.month_total
    );
    if let Some(job) = &report.most_expensive_job {
        let _ = writeln!(
            output,
            "\n最贵的 job：{}（{}）${:.2}",
            job.job_id, job.intent, job.cost
        );
    }
    output
}

fn price_for_provider(cost: &crate::config::CostConfig, provider: &str, tokens: u64) -> f64 {
    let avg = match provider {
        "claude" => (cost.claude_input_per_1m + cost.claude_output_per_1m) / 2.0,
        "gemini" => (cost.gemini_input_per_1m + cost.gemini_output_per_1m) / 2.0,
        _ => (cost.codex_input_per_1m + cost.codex_output_per_1m) / 2.0,
    };
    tokens as f64 / 1_000_000.0 * avg
}

fn same_day(lhs: chrono::DateTime<Local>, rhs: chrono::DateTime<Local>) -> bool {
    lhs.year() == rhs.year() && lhs.month() == rhs.month() && lhs.day() == rhs.day()
}

fn progress_bar(cost: f64, total: f64) -> String {
    if total <= 0.0 {
        return "░░░░░░░░░░".to_string();
    }
    let filled = ((cost / total) * 10.0).round().clamp(0.0, 10.0) as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled))
}

fn capitalize(provider: &str) -> String {
    let mut chars = provider.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => "Unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ActionLogEntry, Session, Worker, WorkerRole, WorkerStatus};
    use chrono::TimeZone;
    use std::collections::HashMap;

    fn ts(day: u32, hour: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, day, hour, 0, 0).unwrap()
    }

    fn sample_session() -> Session {
        Session {
            id: "sess".to_string(),
            workspace_path: ".".to_string(),
            workspace_hash: "hash".to_string(),
            manager_id: None,
            workers: [(
                "w1".to_string(),
                Worker {
                    id: "w1".to_string(),
                    index: 1,
                    provider: "codex".to_string(),
                    role: WorkerRole::Worker,
                    status: WorkerStatus::Running,
                    job_id: Some("job_001".to_string()),
                    pid: None,
                    pane_id: String::new(),
                    mcp_connected: true,
                    context_usage_pct: None,
                    token_count: None,
                    last_heartbeat: None,
                    last_progress: None,
                    permissions: vec![],
                    started_at: ts(6, 9),
                },
            )]
            .into_iter()
            .collect(),
            jobs: [(
                "job_001".to_string(),
                crate::types::Job {
                    id: "job_001".to_string(),
                    intent: "Implement feature".to_string(),
                    status: crate::types::JobStatus::Running,
                    worker_id: Some("w1".to_string()),
                    depends_on: vec![],
                    created_at: ts(6, 9),
                    updated_at: ts(6, 9),
                    branch: None,
                    branch_start_commit: None,
                    checkpoints: vec![],
                    result: None,
                    fail_count: 0,
                    last_fail_at: None,
                },
            )]
            .into_iter()
            .collect(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: crate::types::GitStrategy::None,
            available_providers: vec![],
            notification_mode: crate::types::NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: ts(6, 9),
        }
    }

    #[test]
    fn test_cost_calculates_from_context_ping() {
        let entries = vec![
            ActionLogEntry {
                timestamp: ts(6, 10),
                actor: "w1".to_string(),
                action: "context.ping".to_string(),
                params: serde_json::json!({"job_id":"job_001","token_count":100_000}),
                result: None,
                error: None,
            },
            ActionLogEntry {
                timestamp: ts(6, 11),
                actor: "w1".to_string(),
                action: "context.ping".to_string(),
                params: serde_json::json!({"job_id":"job_001","token_count":180_000}),
                result: None,
                error: None,
            },
        ];
        let report = build_cost_report(
            &entries,
            Some(&sample_session()),
            &KingdomConfig::default_config().cost,
            ts(6, 12),
        );
        assert!(report.today_total > 0.0);
        assert_eq!(report.providers.len(), 1);
        assert_eq!(report.providers[0].today_tokens, 180_000);
    }

    #[test]
    fn test_cost_handles_compressed_summary() {
        let entries = vec![ActionLogEntry {
            timestamp: ts(6, 8),
            actor: "kingdom".to_string(),
            action: "compressed_summary".to_string(),
            params: serde_json::json!({"tokens": 500_000}),
            result: None,
            error: None,
        }];
        let report = build_cost_report(
            &entries,
            None,
            &KingdomConfig::default_config().cost,
            ts(6, 12),
        );
        assert!(report.today_total > 0.0);
    }
}
