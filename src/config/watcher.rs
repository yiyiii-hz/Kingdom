use crate::config::KingdomConfig;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;

pub async fn reload_if_changed(
    config_path: &PathBuf,
    config: &Arc<RwLock<KingdomConfig>>,
    last_modified: SystemTime,
) -> Option<SystemTime> {
    let meta = std::fs::metadata(config_path).ok()?;
    let modified = meta.modified().ok()?;
    if modified <= last_modified {
        return None;
    }
    let new_cfg = KingdomConfig::load(config_path).ok()?;
    *config.write().await = new_cfg;
    Some(modified)
}

pub async fn config_watcher(config_path: PathBuf, config: Arc<RwLock<KingdomConfig>>) {
    let mut last_modified = SystemTime::UNIX_EPOCH;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if let Some(new_mtime) = reload_if_changed(&config_path, &config, last_modified).await {
            last_modified = new_mtime;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KingdomConfig;
    use std::time::SystemTime;

    #[tokio::test]
    async fn reload_if_changed_picks_up_new_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[idle]
timeout_minutes = 10
"#,
        )
        .unwrap();

        let cfg = Arc::new(RwLock::new(KingdomConfig::default_config()));
        let result = reload_if_changed(&path, &cfg, SystemTime::UNIX_EPOCH).await;
        assert!(result.is_some());
        assert_eq!(cfg.read().await.idle.timeout_minutes, 10);
    }

    #[tokio::test]
    async fn reload_if_changed_skips_when_not_modified() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[idle]\ntimeout_minutes = 20\n").unwrap();

        let cfg = Arc::new(RwLock::new(KingdomConfig::default_config()));
        let mtime = reload_if_changed(&path, &cfg, SystemTime::UNIX_EPOCH)
            .await
            .unwrap();
        let result = reload_if_changed(&path, &cfg, mtime).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn config_watcher_reloads_within_five_seconds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[idle]\ntimeout_minutes = 20\n").unwrap();

        let cfg = Arc::new(RwLock::new(KingdomConfig::default_config()));
        let watcher_cfg = Arc::clone(&cfg);
        let watcher_path = path.clone();
        let task = tokio::spawn(async move {
            config_watcher(watcher_path, watcher_cfg).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::fs::write(&path, "[idle]\ntimeout_minutes = 7\n").unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
        loop {
            if cfg.read().await.idle.timeout_minutes == 7 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        task.abort();
    }
}
