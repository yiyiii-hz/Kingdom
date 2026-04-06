use crate::config::FailoverConfig;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

pub struct CircuitBreaker {
    failure_records: HashMap<String, Vec<DateTime<Utc>>>,
    last_failover_at: HashMap<String, DateTime<Utc>>,
    config: FailoverConfig,
}

impl CircuitBreaker {
    pub fn new(config: FailoverConfig) -> Self {
        Self {
            failure_records: HashMap::new(),
            last_failover_at: HashMap::new(),
            config,
        }
    }

    pub fn update_config(&mut self, config: FailoverConfig) {
        self.config = config;
    }

    pub fn record_failure(&mut self, job_id: &str, now: DateTime<Utc>) -> CircuitBreakerResult {
        let entry = self.failure_records.entry(job_id.to_string()).or_default();
        entry.push(now);
        let window = Duration::minutes(self.config.window_minutes as i64);
        entry.retain(|ts| now.signed_duration_since(*ts) <= window);
        if entry.len() as u32 >= self.config.failure_threshold {
            CircuitBreakerResult::Tripped
        } else {
            CircuitBreakerResult::Ok
        }
    }

    pub fn note_failover(&mut self, worker_id: &str, now: DateTime<Utc>) {
        self.last_failover_at.insert(worker_id.to_string(), now);
    }

    pub fn check_cooldown(
        &self,
        worker_id: &str,
        now: DateTime<Utc>,
    ) -> Option<std::time::Duration> {
        let last = self.last_failover_at.get(worker_id)?;
        let elapsed = now.signed_duration_since(*last);
        let cooldown = Duration::seconds(self.config.cooldown_seconds as i64);
        if elapsed >= cooldown {
            return None;
        }
        (cooldown - elapsed).to_std().ok()
    }
}

pub enum CircuitBreakerResult {
    Ok,
    Tripped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trips_after_threshold_within_window() {
        let mut breaker = CircuitBreaker::new(FailoverConfig::default());
        let now = Utc::now();
        assert!(matches!(
            breaker.record_failure("job_1", now),
            CircuitBreakerResult::Ok
        ));
        assert!(matches!(
            breaker.record_failure("job_1", now + Duration::minutes(1)),
            CircuitBreakerResult::Ok
        ));
        assert!(matches!(
            breaker.record_failure("job_1", now + Duration::minutes(2)),
            CircuitBreakerResult::Tripped
        ));
    }

    #[test]
    fn cooldown_returns_remaining_time() {
        let mut breaker = CircuitBreaker::new(FailoverConfig::default());
        let now = Utc::now();
        breaker.note_failover("w1", now);
        let remaining = breaker
            .check_cooldown("w1", now + Duration::seconds(5))
            .unwrap();
        assert!(remaining.as_secs() <= 25);
        assert!(remaining.as_secs() >= 24);
        assert!(breaker
            .check_cooldown("w1", now + Duration::seconds(35))
            .is_none());
    }
}
