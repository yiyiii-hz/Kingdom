use crate::config::KingdomConfig;
use std::path::PathBuf;
use std::process::Command;

pub struct DetectedProvider {
    pub name: String,
    pub binary: PathBuf,
    pub api_key_set: bool,
}

pub struct ProviderDiscovery;

const KNOWN_PROVIDERS: &[(&str, &str)] = &[
    ("claude", "ANTHROPIC_API_KEY"),
    ("codex", "OPENAI_API_KEY"),
    ("gemini", "GEMINI_API_KEY"),
];

impl ProviderDiscovery {
    pub fn detect(config: &KingdomConfig) -> Vec<DetectedProvider> {
        KNOWN_PROVIDERS
            .iter()
            .filter_map(|(name, key_env)| {
                let binary = Self::check(name, config)?;
                Some(DetectedProvider {
                    name: name.to_string(),
                    binary,
                    // Informational only — provider is usable if binary is installed,
                    // regardless of whether an env key is set (CLI auth is also valid).
                    api_key_set: std::env::var(key_env)
                        .map(|v| !v.is_empty())
                        .unwrap_or(false),
                })
            })
            .collect()
    }

    pub fn check(provider: &str, config: &KingdomConfig) -> Option<PathBuf> {
        if let Some(override_path) = config.providers.overrides.get(provider) {
            // Override is explicit: honour it strictly.
            // A non-existent override path means the provider is intentionally excluded.
            let p = PathBuf::from(override_path);
            return p.exists().then_some(p);
        }
        Self::which(provider)
    }

    pub fn check_api_key(provider: &str) -> bool {
        KNOWN_PROVIDERS
            .iter()
            .find(|(name, _)| *name == provider)
            .map(|(_, env)| std::env::var(env).map(|v| !v.is_empty()).unwrap_or(false))
            .unwrap_or(false)
    }

    fn which(binary: &str) -> Option<PathBuf> {
        let output = Command::new("which").arg(binary).output().ok()?;
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KingdomConfig;

    #[test]
    fn check_nonexistent_provider() {
        let cfg = KingdomConfig::default_config();
        assert!(ProviderDiscovery::check("nonexistent_provider_xyz", &cfg).is_none());
    }

    #[test]
    fn check_api_key_unknown_provider() {
        assert!(!ProviderDiscovery::check_api_key("unknown_xyz"));
    }

    #[test]
    fn detect_returns_list_without_panicking() {
        let cfg = KingdomConfig::default_config();
        let _providers = ProviderDiscovery::detect(&cfg);
    }
}
