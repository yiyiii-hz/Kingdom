use crate::types::{FailoverReason, ProviderStability, Session};
use chrono::{DateTime, Utc};

pub fn record_failure(
    session: &mut Session,
    provider: &str,
    reason: &FailoverReason,
    now: DateTime<Utc>,
) {
    let entry = session
        .provider_stability
        .entry(provider.to_string())
        .or_insert_with(|| ProviderStability {
            provider: provider.to_string(),
            ..ProviderStability::default()
        });

    match reason {
        FailoverReason::ProcessExit { .. } => entry.crash_count += 1,
        FailoverReason::HeartbeatTimeout => entry.timeout_count += 1,
        _ => {}
    }

    entry.last_failure_at = Some(now);
}

pub fn failure_score(stability: Option<&ProviderStability>) -> u32 {
    stability
        .map(|s| s.crash_count.saturating_add(s.timeout_count))
        .unwrap_or(0)
}

pub fn provider_priority(provider: &str) -> u8 {
    match provider {
        "claude" => 0,
        "codex" => 1,
        "gemini" => 2,
        _ => 3,
    }
}

pub fn sort_by_stability(candidates: &[String], session: &Session) -> Vec<String> {
    let mut sorted = candidates.to_vec();
    sorted.sort_by_key(|provider| {
        (
            failure_score(session.provider_stability.get(provider)),
            provider_priority(provider),
            provider.clone(),
        )
    });
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GitStrategy, NotificationMode, Session};
    use std::collections::HashMap;

    fn session() -> Session {
        Session {
            id: "sess_1".to_string(),
            workspace_path: "/tmp".to_string(),
            workspace_hash: "abc".to_string(),
            manager_id: None,
            workers: HashMap::new(),
            jobs: HashMap::new(),
            notes: vec![],
            worker_seq: 0,
            job_seq: 0,
            request_seq: 0,
            git_strategy: GitStrategy::None,
            available_providers: vec![],
            notification_mode: NotificationMode::Poll,
            pending_requests: HashMap::new(),
            pending_failovers: HashMap::new(),
            provider_stability: HashMap::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn record_failure_updates_crash_and_timeout_counts() {
        let mut session = session();
        let now = Utc::now();

        record_failure(
            &mut session,
            "codex",
            &FailoverReason::ProcessExit { exit_code: 1 },
            now,
        );
        record_failure(
            &mut session,
            "codex",
            &FailoverReason::HeartbeatTimeout,
            now,
        );

        let stability = &session.provider_stability["codex"];
        assert_eq!(stability.crash_count, 1);
        assert_eq!(stability.timeout_count, 1);
        assert_eq!(stability.last_failure_at, Some(now));
    }

    #[test]
    fn sort_prefers_low_failure_score_then_provider_priority() {
        let mut session = session();
        session.provider_stability.insert(
            "codex".to_string(),
            ProviderStability {
                provider: "codex".to_string(),
                crash_count: 2,
                timeout_count: 0,
                last_failure_at: None,
            },
        );
        let sorted = sort_by_stability(
            &[
                "gemini".to_string(),
                "codex".to_string(),
                "claude".to_string(),
            ],
            &session,
        );
        assert_eq!(sorted, vec!["claude", "gemini", "codex"]);
    }
}
