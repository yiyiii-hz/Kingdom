use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(300);

pub struct RecentCalls {
    calls: HashMap<(String, String), (Instant, Value)>,
}

impl Default for RecentCalls {
    fn default() -> Self {
        Self::new()
    }
}

impl RecentCalls {
    pub fn new() -> Self {
        Self {
            calls: HashMap::new(),
        }
    }

    pub fn check(&self, worker_id: &str, jsonrpc_id: &str) -> Option<&Value> {
        let now = Instant::now();
        self.calls
            .get(&(worker_id.to_string(), jsonrpc_id.to_string()))
            .and_then(|(created_at, value)| {
                (now.duration_since(*created_at) <= TTL).then_some(value)
            })
    }

    pub fn insert(&mut self, worker_id: &str, jsonrpc_id: &str, result: Value) {
        self.evict_expired();
        self.calls.insert(
            (worker_id.to_string(), jsonrpc_id.to_string()),
            (Instant::now(), result),
        );
    }

    fn evict_expired(&mut self) {
        let now = Instant::now();
        self.calls
            .retain(|_, (created_at, _)| now.duration_since(*created_at) <= TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::{RecentCalls, TTL};
    use serde_json::json;
    use std::time::Instant;

    #[test]
    fn check_returns_none_for_unknown_key() {
        let calls = RecentCalls::new();
        assert!(calls.check("w1", "1").is_none());
    }

    #[test]
    fn insert_then_check_returns_cached_value() {
        let mut calls = RecentCalls::new();
        calls.insert("w1", "1", json!({"ok": true}));
        assert_eq!(calls.check("w1", "1"), Some(&json!({"ok": true})));
    }

    #[test]
    fn expired_entries_are_not_returned() {
        let mut calls = RecentCalls::new();
        calls.calls.insert(
            ("w1".to_string(), "1".to_string()),
            (
                Instant::now() - TTL - std::time::Duration::from_secs(1),
                json!(null),
            ),
        );
        assert!(calls.check("w1", "1").is_none());
    }

    #[test]
    fn evict_expired_removes_only_expired_entries() {
        let mut calls = RecentCalls::new();
        calls.calls.insert(
            ("w1".to_string(), "expired".to_string()),
            (
                Instant::now() - TTL - std::time::Duration::from_secs(1),
                json!(1),
            ),
        );
        calls.calls.insert(
            ("w1".to_string(), "fresh".to_string()),
            (Instant::now(), json!(2)),
        );

        calls.evict_expired();

        assert!(!calls
            .calls
            .contains_key(&("w1".to_string(), "expired".to_string())));
        assert!(calls
            .calls
            .contains_key(&("w1".to_string(), "fresh".to_string())));
    }
}
