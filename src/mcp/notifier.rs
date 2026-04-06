use crate::tmux::TmuxController;
use crate::types::{FailoverReason, ManagerNotification, NotificationMode};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, NotifierError>;

#[derive(Debug)]
pub enum NotifierError {
    Json(serde_json::Error),
}

impl Display for NotifierError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for NotifierError {}

impl From<serde_json::Error> for NotifierError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub struct ManagerNotifier {
    pub manager_pane_id: Option<String>,
    pub notification_mode: NotificationMode,
    pending_queue: VecDeque<ManagerNotification>,
    tmux: Option<Arc<TmuxController>>,
}

impl ManagerNotifier {
    pub fn new(
        manager_pane_id: Option<String>,
        notification_mode: NotificationMode,
        tmux: Option<Arc<TmuxController>>,
    ) -> Self {
        Self {
            manager_pane_id,
            notification_mode,
            pending_queue: VecDeque::new(),
            tmux,
        }
    }

    pub async fn push(&mut self, notification: ManagerNotification) -> Result<()> {
        let text = Self::format_notification(&notification);
        if let (Some(tmux), Some(pane_id)) = (&self.tmux, &self.manager_pane_id) {
            let _ = tmux.inject_line(pane_id, &text);
        }

        if matches!(self.notification_mode, NotificationMode::Poll) {
            self.pending_queue.push_back(notification);
        } else {
            let _ = Self::to_mcp_event(&notification);
        }
        Ok(())
    }

    pub async fn flush_queue(&mut self) -> Result<()> {
        while let Some(notification) = self.pending_queue.pop_front() {
            if let (Some(tmux), Some(pane_id)) = (&self.tmux, &self.manager_pane_id) {
                let _ = tmux.inject_line(pane_id, &Self::format_notification(&notification));
            }
        }
        Ok(())
    }

    pub fn format_notification(notification: &ManagerNotification) -> String {
        match notification {
            ManagerNotification::JobCompleted {
                job_id,
                worker_id,
                summary,
                changed_files,
            } => format!(
                "[Kingdom] {} 已完成\n  worker: {}\n  摘要: {}\n  changed: {}\n  → 调用 job.result(\"{}\") 查看完整结果",
                job_id,
                worker_id,
                sanitize_text(summary),
                if changed_files.is_empty() {
                    "-".to_string()
                } else {
                    changed_files.join(", ")
                },
                job_id
            ),
            ManagerNotification::JobFailed {
                job_id,
                worker_id,
                reason,
            } => format!(
                "[Kingdom] {} 失败\n  worker: {}\n  原因: {}\n  → 调用 failover.confirm(\"{}\", ...) 或 failover.cancel(\"{}\")",
                job_id,
                worker_id,
                sanitize_text(reason),
                worker_id,
                worker_id
            ),
            ManagerNotification::WorkerRequest {
                job_id,
                request_id,
                question,
                ..
            } => format!(
                "[Kingdom] worker 需要你的回应\n  job: {}\n  request: {}\n  问题: {}\n  → 调用 worker.respond(..., \"{}\", ...)",
                job_id,
                request_id,
                sanitize_text(question),
                request_id
            ),
            ManagerNotification::JobUnblocked { job_id } => {
                format!("[Kingdom] {} 已解除阻塞", job_id)
            }
            ManagerNotification::FailoverReady {
                worker_id,
                reason,
                candidates,
            } => format!(
                "[Kingdom] worker {} 已准备 failover\n  原因: {}\n  candidates: {}\n  → 调用 failover.confirm(\"{}\", ...)",
                worker_id,
                format_reason(reason),
                if candidates.is_empty() {
                    "-".to_string()
                } else {
                    candidates.join(", ")
                },
                worker_id
            ),
            ManagerNotification::WorkerIdle { worker_id } => {
                format!("[Kingdom] worker {} 当前空闲", worker_id)
            }
            ManagerNotification::WorkerReady {
                worker_id,
                provider,
            } => format!(
                "[Kingdom] worker {} 已恢复并连接\n  provider: {}",
                worker_id, provider
            ),
            ManagerNotification::SubtaskCreated {
                parent_job_id,
                subtask_job_id,
                intent,
            } => format!(
                "[Kingdom] 已创建子任务\n  parent: {}\n  subtask: {}\n  intent: {}",
                parent_job_id,
                subtask_job_id,
                sanitize_text(intent)
            ),
            ManagerNotification::CancelCascade {
                cancelled_job_id,
                affected_jobs,
            } => format!(
                "[Kingdom] {} 触发级联取消\n  affected: {}",
                cancelled_job_id,
                if affected_jobs.is_empty() {
                    "-".to_string()
                } else {
                    affected_jobs.join(", ")
                }
            ),
            ManagerNotification::ProgressWarning {
                worker_id,
                job_id,
                elapsed_minutes,
            } => format!(
                "[Kingdom] 进度超时警告\n  worker: {}\n  job: {}\n  已经过 {} 分钟没有进展",
                worker_id, job_id, elapsed_minutes
            ),
        }
    }

    pub fn to_mcp_event(notification: &ManagerNotification) -> Value {
        let (kind, data) = match notification {
            ManagerNotification::JobCompleted {
                job_id,
                worker_id,
                changed_files,
                ..
            } => (
                "job_completed",
                json!({
                    "job_id": job_id,
                    "worker_id": worker_id,
                    "changed_files": changed_files,
                }),
            ),
            ManagerNotification::JobFailed {
                job_id,
                worker_id,
                reason,
            } => (
                "job_failed",
                json!({
                    "job_id": job_id,
                    "worker_id": worker_id,
                    "reason": reason,
                }),
            ),
            ManagerNotification::WorkerRequest {
                job_id,
                request_id,
                blocking,
                ..
            } => (
                "worker_request",
                json!({
                    "job_id": job_id,
                    "request_id": request_id,
                    "blocking": blocking,
                }),
            ),
            ManagerNotification::JobUnblocked { job_id } => {
                ("job_unblocked", json!({ "job_id": job_id }))
            }
            ManagerNotification::FailoverReady {
                worker_id,
                candidates,
                ..
            } => (
                "failover_ready",
                json!({
                    "worker_id": worker_id,
                    "candidates": candidates,
                }),
            ),
            ManagerNotification::WorkerIdle { worker_id } => {
                ("worker_idle", json!({ "worker_id": worker_id }))
            }
            ManagerNotification::WorkerReady {
                worker_id,
                provider,
            } => (
                "worker_ready",
                json!({ "worker_id": worker_id, "provider": provider }),
            ),
            ManagerNotification::SubtaskCreated {
                parent_job_id,
                subtask_job_id,
                ..
            } => (
                "subtask_created",
                json!({
                    "parent_job_id": parent_job_id,
                    "subtask_job_id": subtask_job_id,
                }),
            ),
            ManagerNotification::CancelCascade {
                cancelled_job_id,
                affected_jobs,
            } => (
                "cancel_cascade",
                json!({
                    "cancelled_job_id": cancelled_job_id,
                    "affected_jobs": affected_jobs,
                }),
            ),
            ManagerNotification::ProgressWarning {
                worker_id,
                job_id,
                elapsed_minutes,
            } => (
                "progress_warning",
                json!({
                    "worker_id": worker_id,
                    "job_id": job_id,
                    "elapsed_minutes": elapsed_minutes,
                }),
            ),
        };

        json!({
            "type": kind,
            "data": data,
            "text": Self::format_notification(notification),
        })
    }
}

fn sanitize_text(input: &str) -> String {
    let trimmed = truncate_to_2kb(input);
    let lower = trimmed.trim_start().to_ascii_lowercase();
    if lower.starts_with("system:") || lower.starts_with("<system>") {
        tracing::warn!("suspicious system-prefixed notification text observed");
    }
    trimmed
}

fn truncate_to_2kb(input: &str) -> String {
    if input.len() <= 2048 {
        return input.to_string();
    }
    let mut end = 2048;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[已截断]", &input[..end])
}

fn format_reason(reason: &FailoverReason) -> String {
    match reason {
        FailoverReason::Network => "Network".to_string(),
        FailoverReason::ContextLimit => "ContextLimit".to_string(),
        FailoverReason::ProcessExit { exit_code } => format!("ProcessExit({exit_code})"),
        FailoverReason::HeartbeatTimeout => "HeartbeatTimeout".to_string(),
        FailoverReason::RateLimit => "RateLimit".to_string(),
        FailoverReason::Manual => "Manual".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_notification_truncates_summary() {
        let text = ManagerNotifier::format_notification(&ManagerNotification::JobCompleted {
            job_id: "job_1".to_string(),
            worker_id: "w1".to_string(),
            summary: "a".repeat(3000),
            changed_files: vec![],
        });
        assert!(text.contains("已截断"));
    }

    #[test]
    fn to_mcp_event_has_expected_shape() {
        let event = ManagerNotifier::to_mcp_event(&ManagerNotification::JobCompleted {
            job_id: "job_1".to_string(),
            worker_id: "w1".to_string(),
            summary: "done".to_string(),
            changed_files: vec!["src/lib.rs".to_string()],
        });
        assert_eq!(event["type"], "job_completed");
        assert_eq!(event["data"]["job_id"], "job_1");
        assert_eq!(event["data"]["changed_files"][0], "src/lib.rs");
    }

    #[tokio::test]
    async fn poll_mode_queues_notifications() {
        let mut notifier = ManagerNotifier::new(None, NotificationMode::Poll, None);
        notifier
            .push(ManagerNotification::WorkerIdle {
                worker_id: "w1".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(notifier.pending_queue.len(), 1);
    }
}
