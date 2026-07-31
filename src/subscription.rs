// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_PROFILE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct SubscriptionService {
    client: reqwest::Client,
}

pub struct StagedProfile {
    target: PathBuf,
    backup: Option<PathBuf>,
}

impl SubscriptionService {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("slint-clash/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
        }
    }

    pub async fn download(
        &self,
        url: &str,
        config_dir: &Path,
        profile_name: &str,
        overwrite: bool,
    ) -> eyre::Result<StagedProfile> {
        validate_profile_name(profile_name)?;
        let parsed_url = reqwest::Url::parse(url)?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(eyre::eyre!("Subscription URL must use HTTP or HTTPS"));
        }
        if !parsed_url.username().is_empty() || parsed_url.password().is_some() {
            return Err(eyre::eyre!(
                "Subscription URL credentials are not supported; use a tokenized URL"
            ));
        }

        let mut response = self
            .client
            .get(parsed_url)
            .send()
            .await?
            .error_for_status()?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PROFILE_SIZE as u64)
        {
            return Err(eyre::eyre!("Subscription is larger than 16 MiB"));
        }

        let mut bytes = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or(64 * 1024)
                .min(MAX_PROFILE_SIZE as u64) as usize,
        );
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > MAX_PROFILE_SIZE {
                return Err(eyre::eyre!("Subscription is larger than 16 MiB"));
            }
            bytes.extend_from_slice(&chunk);
        }
        validate_yaml(&bytes)?;

        std::fs::create_dir_all(config_dir)?;
        let target = config_dir.join(format!("{profile_name}.yaml"));
        if target.exists() && !overwrite {
            return Err(eyre::eyre!("Profile already exists: {profile_name}"));
        }

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = config_dir.join(format!(".{profile_name}.{unique}.download"));
        let backup = target
            .exists()
            .then(|| config_dir.join(format!(".{profile_name}.{unique}.backup")));
        std::fs::write(&temporary, bytes)?;

        if let Some(backup) = &backup {
            if let Err(error) = std::fs::rename(&target, backup) {
                let _ = std::fs::remove_file(&temporary);
                return Err(error.into());
            }
        }
        if let Err(error) = std::fs::rename(&temporary, &target) {
            if let Some(backup) = &backup {
                let _ = std::fs::rename(backup, &target);
            }
            let _ = std::fs::remove_file(&temporary);
            return Err(error.into());
        }

        Ok(StagedProfile { target, backup })
    }
}

impl StagedProfile {
    pub fn path(&self) -> &Path {
        &self.target
    }

    pub fn commit(mut self) -> eyre::Result<()> {
        if let Some(backup) = self.backup.take() {
            std::fs::remove_file(backup)?;
        }
        Ok(())
    }

    pub fn rollback(mut self) -> eyre::Result<()> {
        if self.target.exists() {
            std::fs::remove_file(&self.target)?;
        }
        if let Some(backup) = self.backup.take() {
            std::fs::rename(backup, &self.target)?;
        }
        Ok(())
    }
}

fn validate_profile_name(name: &str) -> eyre::Result<()> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(eyre::eyre!("Profile name must contain 1 to 64 characters"));
    }
    if matches!(name, "." | "..")
        || name.ends_with([' ', '.'])
        || is_windows_reserved_name(name)
        || name.chars().any(|character| {
            matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
        })
    {
        return Err(eyre::eyre!(
            "Profile name contains characters that cannot be used in a file name"
        ));
    }
    Ok(())
}

fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}

fn validate_yaml(bytes: &[u8]) -> eyre::Result<()> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| eyre::eyre!("Subscription response is not valid UTF-8"))?;
    if text.trim().is_empty() {
        return Err(eyre::eyre!("Subscription response is empty"));
    }
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(text)
        .map_err(|error| eyre::eyre!("Subscription is not valid YAML: {error}"))?;
    if !value.is_mapping() {
        return Err(eyre::eyre!(
            "Subscription YAML must contain a top-level mapping"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn serve_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/yaml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}/profile.yaml")
    }

    #[test]
    fn validates_profile_names_for_cross_platform_file_safety() {
        assert!(validate_profile_name("Work 2026").is_ok());
        assert!(validate_profile_name("../escape").is_err());
        assert!(validate_profile_name("bad:name").is_err());
        assert!(validate_profile_name("trailing.").is_err());
        assert!(validate_profile_name("CON.yaml").is_err());
    }

    #[test]
    fn validates_yaml_before_it_can_replace_a_profile() {
        assert!(validate_yaml(b"proxies:\n  - name: example\n").is_ok());
        assert!(validate_yaml(b"<html>not a subscription</html>").is_err());
        assert!(validate_yaml(b"").is_err());
    }

    #[tokio::test]
    async fn downloads_and_commits_a_valid_subscription() {
        let directory =
            std::env::temp_dir().join(format!("slint-clash-download-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let url = serve_once("proxies:\n  - name: example\n").await;

        let staged = SubscriptionService::new()
            .download(&url, &directory, "Work", false)
            .await
            .unwrap();
        assert!(staged.path().is_file());
        staged.commit().unwrap();
        assert!(std::fs::read_to_string(directory.join("Work.yaml"))
            .unwrap()
            .contains("example"));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn rolls_back_an_updated_subscription() {
        let directory =
            std::env::temp_dir().join(format!("slint-clash-rollback-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("Work.yaml"), "proxies: []\n").unwrap();
        let url = serve_once("proxies:\n  - name: replacement\n").await;

        let staged = SubscriptionService::new()
            .download(&url, &directory, "Work", true)
            .await
            .unwrap();
        assert!(std::fs::read_to_string(staged.path())
            .unwrap()
            .contains("replacement"));
        staged.rollback().unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.join("Work.yaml")).unwrap(),
            "proxies: []\n"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }
}
