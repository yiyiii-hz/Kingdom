use crate::config::WebhookConfig;
use crate::storage::Storage;
use crate::types::{ActionLogEntry, ManagerNotification};
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
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            config,
            workspace_path,
            storage,
            client,
        }
    }

    pub fn event_name(notification: &ManagerNotification) -> Option<&'static str> {
        match notification {
            ManagerNotification::JobCompleted { .. } => Some("job.completed"),
            ManagerNotification::JobFailed { .. } => Some("job.failed"),
            ManagerNotification::FailoverReady { .. } => Some("failover.triggered"),
            _ => None,
        }
    }

    fn is_subscribed(&self, event: &str) -> bool {
        self.config
            .events
            .iter()
            .any(|candidate| candidate == event)
    }

    pub fn build_payload(notification: &ManagerNotification, workspace: &str) -> serde_json::Value {
        use serde_json::json;

        match notification {
            ManagerNotification::JobCompleted {
                job_id,
                worker_id,
                summary,
                ..
            } => json!({
                "event": "job.completed",
                "job_id": job_id,
                "worker": worker_id,
                "summary": summary,
                "workspace": workspace,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ManagerNotification::JobFailed {
                job_id,
                worker_id,
                reason,
            } => json!({
                "event": "job.failed",
                "job_id": job_id,
                "worker": worker_id,
                "reason": reason,
                "workspace": workspace,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
            ManagerNotification::FailoverReady {
                worker_id,
                reason,
                candidates,
            } => json!({
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

    pub async fn send(&self, notification: &ManagerNotification) {
        let url = match &self.config.url {
            Some(url) if !url.is_empty() => url.clone(),
            _ => return,
        };
        let event = match Self::event_name(notification) {
            Some(event) => event,
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
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                let status = response.status().as_u16();
                tracing::warn!(url, status, "webhook returned non-2xx, skipping");
                self.log_warning(format!("webhook {url} returned {status}"));
            }
            Err(error) => {
                tracing::warn!(url, error = %error, "webhook failed, skipping");
                self.log_warning(format!("webhook {url} failed: {error}"));
            }
        }
    }

    fn log_warning(&self, message: String) {
        let entry = ActionLogEntry {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FailoverReason, ManagerNotification};

    #[test]
    fn test_webhook_event_name_mapping() {
        assert_eq!(
            WebhookNotifier::event_name(&ManagerNotification::JobCompleted {
                job_id: "job_001".to_string(),
                worker_id: "w1".to_string(),
                summary: "done".to_string(),
                changed_files: vec![],
            }),
            Some("job.completed")
        );
        assert_eq!(
            WebhookNotifier::event_name(&ManagerNotification::JobFailed {
                job_id: "job_001".to_string(),
                worker_id: "w1".to_string(),
                reason: "boom".to_string(),
            }),
            Some("job.failed")
        );
        assert_eq!(
            WebhookNotifier::event_name(&ManagerNotification::WorkerIdle {
                worker_id: "w1".to_string()
            }),
            None
        );
    }

    #[test]
    fn test_webhook_is_subscribed() {
        let temp = tempfile::tempdir().unwrap();
        let notifier = WebhookNotifier::new(
            WebhookConfig {
                url: None,
                events: vec!["job.completed".to_string()],
                timeout_seconds: 5,
            },
            temp.path().display().to_string(),
            Arc::new(Storage::init(temp.path()).unwrap()),
        );
        assert!(notifier.is_subscribed("job.completed"));
        assert!(!notifier.is_subscribed("job.failed"));
    }

    #[test]
    fn test_webhook_build_payload_job_completed() {
        let payload = WebhookNotifier::build_payload(
            &ManagerNotification::JobCompleted {
                job_id: "job_001".to_string(),
                worker_id: "w1".to_string(),
                summary: "done".to_string(),
                changed_files: vec!["src/lib.rs".to_string()],
            },
            "/tmp/demo",
        );
        assert_eq!(payload["event"], "job.completed");
        assert_eq!(payload["job_id"], "job_001");
        assert_eq!(payload["worker"], "w1");
        assert_eq!(payload["summary"], "done");
        assert_eq!(payload["workspace"], "/tmp/demo");
        assert!(payload["timestamp"].as_str().is_some());
    }

    #[tokio::test]
    async fn test_webhook_url_empty_skips_silently() {
        let temp = tempfile::tempdir().unwrap();
        let notifier = WebhookNotifier::new(
            WebhookConfig {
                url: None,
                events: vec!["job.completed".to_string()],
                timeout_seconds: 5,
            },
            temp.path().display().to_string(),
            Arc::new(Storage::init(temp.path()).unwrap()),
        );
        notifier
            .send(&ManagerNotification::FailoverReady {
                worker_id: "w1".to_string(),
                reason: FailoverReason::Manual,
                candidates: vec!["codex".to_string()],
            })
            .await;
    }
}
