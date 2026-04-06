use crate::failover::stability::provider_priority;
use crate::types::{FailoverReason, Session};

pub fn recommend_provider(
    failed_provider: &str,
    available_providers: &[String],
    failure_reason: &FailoverReason,
    session_failures: &[String],
    manager_provider: &str,
    session: &Session,
) -> Option<String> {
    let mut candidates: Vec<String> = available_providers
        .iter()
        .filter(|provider| provider.as_str() != failed_provider)
        .filter(|provider| !session_failures.iter().any(|failed| failed == *provider))
        .filter(|provider| provider.as_str() != manager_provider)
        .cloned()
        .collect();

    if candidates.is_empty() {
        return None;
    }

    if matches!(failure_reason, FailoverReason::ContextLimit)
        && candidates.iter().any(|provider| provider == "claude")
    {
        return Some("claude".to_string());
    }

    candidates.sort_by_key(|provider| {
        (
            session
                .provider_stability
                .get(provider)
                .map(|s| s.crash_count + s.timeout_count)
                .unwrap_or(0),
            provider_priority(provider),
            provider.clone(),
        )
    });
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GitStrategy, NotificationMode, ProviderStability, Session};
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
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn recommend_excludes_manager_provider() {
        let session = session();
        let got = recommend_provider(
            "codex",
            &[
                "claude".to_string(),
                "codex".to_string(),
                "gemini".to_string(),
            ],
            &FailoverReason::ProcessExit { exit_code: 1 },
            &[],
            "claude",
            &session,
        );
        assert_eq!(got, Some("gemini".to_string()));
    }

    #[test]
    fn recommend_prefers_claude_for_context_limit() {
        let session = session();
        let got = recommend_provider(
            "codex",
            &["claude".to_string(), "codex".to_string()],
            &FailoverReason::ContextLimit,
            &[],
            "gemini",
            &session,
        );
        assert_eq!(got, Some("claude".to_string()));
    }

    #[test]
    fn recommend_returns_none_when_candidates_exhausted() {
        let session = session();
        let got = recommend_provider(
            "claude",
            &["claude".to_string()],
            &FailoverReason::ProcessExit { exit_code: 1 },
            &[],
            "n/a",
            &session,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn recommend_respects_session_failures() {
        let session = session();
        let got = recommend_provider(
            "codex",
            &["claude".to_string(), "codex".to_string()],
            &FailoverReason::ProcessExit { exit_code: 1 },
            &["claude".to_string()],
            "gemini",
            &session,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn recommend_prefers_more_stable_provider() {
        let mut session = session();
        session.provider_stability.insert(
            "claude".to_string(),
            ProviderStability {
                provider: "claude".to_string(),
                crash_count: 2,
                timeout_count: 0,
                last_failure_at: None,
            },
        );
        let got = recommend_provider(
            "codex",
            &[
                "claude".to_string(),
                "gemini".to_string(),
                "codex".to_string(),
            ],
            &FailoverReason::ProcessExit { exit_code: 1 },
            &[],
            "n/a",
            &session,
        );
        assert_eq!(got, Some("gemini".to_string()));
    }
}
