pub mod watcher;

use std::path::Path;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KingdomConfig {
    #[serde(default)]
    pub tmux: TmuxConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub idle: IdleConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TmuxConfig {
    pub session_name: String,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            session_name: "kingdom".to_string(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub overrides: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IdleConfig {
    pub timeout_minutes: u64,
}

impl Default for IdleConfig {
    fn default() -> Self {
        Self {
            timeout_minutes: 30,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NotificationsConfig {
    pub mode: String,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            mode: "poll".to_string(),
        }
    }
}

impl KingdomConfig {
    pub fn default_config() -> Self {
        Self {
            tmux: TmuxConfig::default(),
            providers: ProvidersConfig::default(),
            idle: IdleConfig::default(),
            notifications: NotificationsConfig::default(),
        }
    }

    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn load_or_default(path: &Path) -> Self {
        Self::load(path).unwrap_or_else(|_| Self::default_config())
    }
}

pub fn workspace_hash(workspace_path: &Path) -> String {
    let canonical = workspace_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_path.to_path_buf());
    let s = canonical.to_string_lossy();
    let mut h: u32 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u32);
    }
    format!("{:06x}", h & 0x00ff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_hash_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let h1 = workspace_hash(tmp.path());
        let h2 = workspace_hash(tmp.path());
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 6);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn config_defaults() {
        let cfg = KingdomConfig::default_config();
        assert_eq!(cfg.tmux.session_name, "kingdom");
        assert_eq!(cfg.idle.timeout_minutes, 30);
        assert_eq!(cfg.notifications.mode, "poll");
    }

    #[test]
    fn config_load_or_default_missing_file() {
        let cfg = KingdomConfig::load_or_default(std::path::Path::new("/nonexistent/config.toml"));
        assert_eq!(cfg.idle.timeout_minutes, 30);
    }
}
