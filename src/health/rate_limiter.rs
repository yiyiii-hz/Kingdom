use std::collections::HashMap;

pub enum RateLimitResult {
    Retrying { wait_secs: u64 },
    Exhausted,
}

pub struct RateLimitHandler {
    retry_counts: HashMap<String, u32>,
}

impl RateLimitHandler {
    pub fn new() -> Self {
        Self {
            retry_counts: HashMap::new(),
        }
    }

    pub fn handle(&mut self, worker_id: &str) -> RateLimitResult {
        let count = self.retry_counts.entry(worker_id.to_string()).or_insert(0);
        *count += 1;
        if *count > 3 {
            self.retry_counts.remove(worker_id);
            return RateLimitResult::Exhausted;
        }

        let wait_secs = match *count {
            1 => 5,
            2 => 15,
            3 => 30,
            _ => 60,
        };
        RateLimitResult::Retrying { wait_secs }
    }

    pub fn reset(&mut self, worker_id: &str) {
        self.retry_counts.remove(worker_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_sequence() {
        let mut h = RateLimitHandler::new();
        assert!(matches!(
            h.handle("w1"),
            RateLimitResult::Retrying { wait_secs: 5 }
        ));
        assert!(matches!(
            h.handle("w1"),
            RateLimitResult::Retrying { wait_secs: 15 }
        ));
        assert!(matches!(
            h.handle("w1"),
            RateLimitResult::Retrying { wait_secs: 30 }
        ));
        assert!(matches!(h.handle("w1"), RateLimitResult::Exhausted));
    }

    #[test]
    fn reset_clears_count() {
        let mut h = RateLimitHandler::new();
        h.handle("w1");
        h.handle("w1");
        h.reset("w1");
        assert!(matches!(
            h.handle("w1"),
            RateLimitResult::Retrying { wait_secs: 5 }
        ));
    }
}
