use crate::types::{
    PendingFailoverStatus, ProviderStability, Session, Worker, WorkerRole, WorkerStatus,
};
use chrono::Utc;

pub fn render_status_bar(session: &Session) -> String {
    let mut parts = Vec::new();
    if let Some(manager_id) = &session.manager_id {
        if let Some(manager) = session.workers.get(manager_id) {
            parts.push(render_manager(manager));
        }
    }

    let mut workers = session
        .workers
        .values()
        .filter(|worker| worker.role == WorkerRole::Worker)
        .collect::<Vec<_>>();
    workers.sort_by_key(|worker| worker.id.clone());

    if workers.len() > 4 {
        for worker in workers.iter().take(3) {
            parts.push(render_compact_worker(session, worker));
        }
        parts.push(format!("[+{}]", workers.len().saturating_sub(3)));
    } else {
        for worker in workers {
            parts.push(render_worker(session, worker));
        }
    }

    parts.push(format!("${:.2}", total_cost_usd(session)));
    parts.push(format_duration(session));
    parts.join("  ")
}

fn render_manager(manager: &Worker) -> String {
    if manager.status == WorkerStatus::Failed && !manager.pane_id.is_empty() {
        "[manager:stale]".to_string()
    } else {
        format!("[{}:mgr]", capitalize(&manager.provider))
    }
}

fn render_worker(session: &Session, worker: &Worker) -> String {
    if worker.status == WorkerStatus::Idle && worker.job_id.is_none() {
        return "[idle]".to_string();
    }
    let icon = worker_icon(session, worker);
    if icon.is_empty() {
        format!("[{}:{}]", capitalize(&worker.provider), worker.id)
    } else {
        format!("[{}:{}{}]", capitalize(&worker.provider), worker.id, icon)
    }
}

fn render_compact_worker(session: &Session, worker: &Worker) -> String {
    let icon = worker_icon(session, worker);
    if icon.is_empty() {
        format!("[{}:⚡]", worker.id)
    } else {
        format!("[{}:{}]", worker.id, icon)
    }
}

fn worker_icon(session: &Session, worker: &Worker) -> &'static str {
    if session
        .pending_failovers
        .get(&worker.id)
        .map(|pending| matches!(pending.status, PendingFailoverStatus::WaitingConfirmation))
        .unwrap_or(false)
    {
        return "⚠";
    }
    if session
        .pending_failovers
        .get(&worker.id)
        .map(|pending| matches!(pending.status, PendingFailoverStatus::Confirmed { .. }))
        .unwrap_or(false)
        || worker.status == WorkerStatus::Starting
    {
        return "↻";
    }
    if is_rate_limited(session.provider_stability.get(&worker.provider)) {
        return "⏳";
    }
    match worker.status {
        WorkerStatus::Failed | WorkerStatus::Terminated => "✗",
        WorkerStatus::Idle if worker.job_id.is_some() => "✓",
        WorkerStatus::Running => "",
        WorkerStatus::Starting => "↻",
        WorkerStatus::Idle => "",
    }
}

fn is_rate_limited(stability: Option<&ProviderStability>) -> bool {
    let Some(stability) = stability else {
        return false;
    };
    stability.timeout_count > 0
        && stability
            .last_failure_at
            .map(|timestamp| Utc::now().signed_duration_since(timestamp).num_minutes() <= 10)
            .unwrap_or(false)
}

fn total_cost_usd(_session: &Session) -> f64 {
    0.0
}

fn format_duration(session: &Session) -> String {
    let elapsed = Utc::now()
        .signed_duration_since(session.created_at)
        .to_std()
        .unwrap_or_default();
    let total_minutes = elapsed.as_secs() / 60;
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    format!("{hours:02}:{minutes:02}")
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
    use crate::types::{
        GitStrategy, Job, JobStatus, NotificationMode, PendingFailover, Session, Worker,
        WorkerRole, WorkspaceNote,
    };
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;

    fn ts() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 5, 12, 0, 0).unwrap()
    }

    fn worker(id: &str, provider: &str, role: WorkerRole, status: WorkerStatus) -> Worker {
        Worker {
            id: id.to_string(),
            index: id
                .strip_prefix('w')
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
            provider: provider.to_string(),
            role,
            status,
            job_id: None,
            pid: None,
            pane_id: format!("%{id}"),
            mcp_connected: true,
            context_usage_pct: None,
            token_count: None,
            last_heartbeat: None,
            last_progress: None,
            permissions: vec![],
            started_at: ts(),
        }
    }

    fn session() -> Session {
        let manager = worker("w0", "claude", WorkerRole::Manager, WorkerStatus::Running);
        Session {
            id: "sess".to_string(),
            workspace_path: ".".to_string(),
            workspace_hash: "hash".to_string(),
            manager_id: Some("w0".to_string()),
            workers: [("w0".to_string(), manager)].into_iter().collect(),
            jobs: HashMap::new(),
            notes: Vec::<WorkspaceNote>::new(),
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: GitStrategy::None,
            available_providers: vec![],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: ts(),
        }
    }

    #[test]
    fn test_render_manager_only() {
        let rendered = render_status_bar(&session());
        assert!(rendered.contains("[Claude:mgr]"));
    }

    #[test]
    fn test_render_worker_statuses() {
        let mut session = session();
        let mut running = worker("w1", "codex", WorkerRole::Worker, WorkerStatus::Running);
        running.job_id = Some("job_1".to_string());
        let mut idle_done = worker("w2", "codex", WorkerRole::Worker, WorkerStatus::Idle);
        idle_done.job_id = Some("job_2".to_string());
        let failed = worker("w3", "codex", WorkerRole::Worker, WorkerStatus::Failed);
        let starting = worker("w4", "codex", WorkerRole::Worker, WorkerStatus::Starting);
        let attention = worker("w5", "gemini", WorkerRole::Worker, WorkerStatus::Running);

        session.workers.extend([
            ("w1".to_string(), running),
            ("w2".to_string(), idle_done),
            ("w3".to_string(), failed),
            ("w4".to_string(), starting),
            ("w5".to_string(), attention.clone()),
        ]);
        session.pending_failovers.insert(
            "w5".to_string(),
            PendingFailover {
                worker_id: "w5".to_string(),
                job_id: "job_5".to_string(),
                reason: crate::types::FailoverReason::HeartbeatTimeout,
                handoff_brief: crate::types::HandoffBrief {
                    job_id: "job_5".to_string(),
                    original_intent: String::new(),
                    done: String::new(),
                    in_progress: String::new(),
                    remaining: String::new(),
                    pitfalls: String::new(),
                    possibly_incomplete_files: vec![],
                    changed_files: vec![],
                },
                recommended_provider: None,
                created_at: ts(),
                status: PendingFailoverStatus::WaitingConfirmation,
            },
        );
        session.workers.remove("w4");

        let rendered = render_status_bar(&session);
        assert!(rendered.contains("[Codex:w1]"));
        assert!(rendered.contains("[Codex:w2✓]"));
        assert!(rendered.contains("[Codex:w3✗]"));
        assert!(rendered.contains("[Gemini:w5⚠]"));
    }

    #[test]
    fn test_render_overflow() {
        let mut session = session();
        for index in 1..=5 {
            let mut worker = worker(
                &format!("w{index}"),
                "codex",
                WorkerRole::Worker,
                WorkerStatus::Idle,
            );
            worker.job_id = Some(format!("job_{index}"));
            session.workers.insert(worker.id.clone(), worker);
            session.jobs.insert(
                format!("job_{index}"),
                Job {
                    id: format!("job_{index}"),
                    intent: format!("Job {index}"),
                    status: JobStatus::Completed,
                    worker_id: Some(format!("w{index}")),
                    depends_on: vec![],
                    created_at: ts(),
                    updated_at: ts(),
                    branch: None,
                    branch_start_commit: None,
                    checkpoints: vec![],
                    result: None,
                    fail_count: 0,
                    last_fail_at: None,
                },
            );
        }
        let rendered = render_status_bar(&session);
        assert!(rendered.contains("[+2]"));
    }
}
