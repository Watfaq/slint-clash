// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub clash_binary_path: Option<String>,
    pub config_dir: Option<String>,
    pub active_profile: Option<String>,
    pub api_port: u16,
    pub api_secret: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clash_binary_path: None,
            config_dir: None,
            active_profile: None,
            api_port: 9090,
            api_secret: None,
        }
    }
}

impl Config {
    pub fn config_dir(&self) -> PathBuf {
        self.config_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::config_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("mihomo")
            })
    }

    pub fn clash_binary(&self) -> PathBuf {
        self.clash_binary_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(detect_default_binary)
    }

    pub fn api_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.api_port)
    }

    pub fn profile_path(&self, profile: &str) -> Option<PathBuf> {
        let file_name = Path::new(profile);
        if file_name.file_name() != Some(file_name.as_os_str()) {
            return None;
        }

        let directory = self.config_dir();
        if matches!(
            file_name.extension().and_then(|value| value.to_str()),
            Some("yaml" | "yml")
        ) {
            let candidate = directory.join(file_name);
            return candidate.is_file().then_some(candidate);
        }

        ["yaml", "yml"]
            .into_iter()
            .map(|extension| directory.join(format!("{profile}.{extension}")))
            .find(|candidate| candidate.is_file())
    }

    pub fn active_config_path(&self) -> PathBuf {
        self.active_profile
            .as_deref()
            .and_then(|profile| self.profile_path(profile))
            .unwrap_or_else(|| self.config_dir().join("config.yaml"))
    }

    pub fn save(&self) -> eyre::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn load() -> eyre::Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let config = serde_json::from_slice(&std::fs::read(path)?)?;
        Ok(config)
    }

    fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("slint-clash")
            .join("config.json")
    }
}

fn detect_default_binary() -> PathBuf {
    let candidates = if cfg!(windows) {
        ["mihomo.exe", "clash-meta.exe", "clash.exe"]
    } else {
        ["mihomo", "clash-meta", "clash"]
    };

    for candidate in candidates {
        if Path::new(candidate).is_file() {
            return PathBuf::from(candidate);
        }
    }

    PathBuf::from(candidates[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_directory() -> PathBuf {
        std::env::temp_dir().join(format!("slint-clash-config-test-{}", std::process::id()))
    }

    #[test]
    fn resolves_only_profiles_inside_the_config_directory() {
        let directory = fixture_directory();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("work.yml"), "mode: rule").unwrap();
        let config = Config {
            config_dir: Some(directory.to_string_lossy().into_owned()),
            ..Config::default()
        };

        assert_eq!(
            config.profile_path("work"),
            Some(directory.join("work.yml"))
        );
        assert_eq!(
            config.profile_path("work.yml"),
            Some(directory.join("work.yml"))
        );
        assert_eq!(config.profile_path("../work"), None);
        assert_eq!(config.profile_path("missing"), None);

        std::fs::remove_dir_all(directory).unwrap();
    }
}
