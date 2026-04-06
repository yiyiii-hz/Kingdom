use crate::tmux::controller::{Result, TmuxController};

pub fn inject_handoff_separator(
    pane_id: &str,
    from_provider: &str,
    to_provider: &str,
    reason: &str,
    brief_summary: &str,
    tmux: &TmuxController,
) -> Result<()> {
    let line = format!(
        "────────────────────────────────────────────────\n\
⚡ HANDOFF  {} → {}                {}\n\
原因: {}\n\
已传递: {}\n\
────────────────────────────────────────────────",
        from_provider,
        to_provider,
        chrono::Utc::now().format("%H:%M:%S"),
        reason,
        brief_summary
    );
    tmux.inject_line(pane_id, &line)
}

pub async fn show_startup_progress(
    pane_id: &str,
    provider: &str,
    tmux: &TmuxController,
    mut connected: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let start = tokio::time::Instant::now();
    loop {
        let line = format!(
            "⏳ 正在启动 {}... ({}s)",
            provider,
            start.elapsed().as_secs()
        );
        tmux.inject_line(pane_id, &line)?;
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {}
            _ = connected.changed() => {
                if *connected.borrow() {
                    break;
                }
            }
        }
        if start.elapsed().as_secs() > 60 {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::controller::testsupport::MockTmux;
    use crate::tmux::TmuxController;
    use std::sync::Arc;

    #[test]
    fn inject_handoff_separator_formats_expected_content() {
        let mock = Arc::new(MockTmux::default());
        let ops: Arc<dyn crate::tmux::controller::TmuxOps> = mock.clone();
        let tmux = TmuxController::with_ops("kingdom", ops);

        inject_handoff_separator(
            "%1",
            "codex",
            "claude",
            "HeartbeatTimeout",
            "resume auth flow",
            &tmux,
        )
        .unwrap();

        let calls = mock.calls.lock().unwrap();
        let sent = calls
            .iter()
            .find(|args| args.first().map(|arg| arg == "send-keys").unwrap_or(false))
            .and_then(|args| args.get(4))
            .cloned()
            .unwrap_or_default();

        assert!(sent.contains("⚡ HANDOFF"));
        assert!(sent.contains("codex"));
        assert!(sent.contains("claude"));
        assert!(sent.contains("HeartbeatTimeout"));
        assert!(sent.contains("resume auth flow"));
        assert!(sent.contains("原因:"));
    }

    #[tokio::test(start_paused = true)]
    async fn show_startup_progress_exits_when_connected_turns_true() {
        let mock = Arc::new(MockTmux::default());
        let ops: Arc<dyn crate::tmux::controller::TmuxOps> = mock.clone();
        let tmux = TmuxController::with_ops("kingdom", ops);
        let (tx, rx) = tokio::sync::watch::channel(false);

        let task =
            tokio::spawn(async move { show_startup_progress("%1", "codex", &tmux, rx).await });
        tokio::task::yield_now().await;
        tx.send(true).unwrap();
        tokio::time::advance(tokio::time::Duration::from_secs(1)).await;

        let result = task.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn show_startup_progress_times_out_after_sixty_seconds() {
        let mock = Arc::new(MockTmux::default());
        let ops: Arc<dyn crate::tmux::controller::TmuxOps> = mock.clone();
        let tmux = TmuxController::with_ops("kingdom", ops);
        let (_tx, rx) = tokio::sync::watch::channel(false);

        let task =
            tokio::spawn(async move { show_startup_progress("%1", "codex", &tmux, rx).await });
        tokio::time::advance(tokio::time::Duration::from_secs(61)).await;

        let result = task.await.unwrap();
        assert!(result.is_ok());
    }
}
