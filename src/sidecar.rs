// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
};

#[derive(Debug)]
pub struct SidecarManager {
    child: Option<Child>,
    binary_path: PathBuf,
    work_dir: PathBuf,
    config_path: PathBuf,
}

impl SidecarManager {
    pub fn new(binary_path: PathBuf, work_dir: PathBuf, config_path: PathBuf) -> Self {
        Self {
            child: None,
            binary_path,
            work_dir,
            config_path,
        }
    }

    pub fn start(&mut self) -> eyre::Result<()> {
        if self.child.is_some() {
            return Err(eyre::eyre!("Mihomo core is already running"));
        }
        if !self.work_dir.is_dir() {
            return Err(eyre::eyre!(
                "Configuration directory does not exist: {}",
                self.work_dir.display()
            ));
        }
        if !self.config_path.is_file() {
            return Err(eyre::eyre!(
                "Configuration file does not exist: {}",
                self.config_path.display()
            ));
        }
        if path_requires_existence_check(&self.binary_path) && !self.binary_path.is_file() {
            return Err(eyre::eyre!(
                "Mihomo binary does not exist: {}",
                self.binary_path.display()
            ));
        }

        tracing::info!(
            binary = ?self.binary_path,
            work_dir = ?self.work_dir,
            config = ?self.config_path,
            "Starting Mihomo core"
        );

        let mut command = Command::new(&self.binary_path);
        command
            .arg("-d")
            .arg(&self.work_dir)
            .arg("-f")
            .arg(&self.config_path)
            .current_dir(&self.work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let child = command.spawn().map_err(|error| {
            eyre::eyre!(
                "Failed to start Mihomo binary '{}': {error}",
                self.binary_path.display()
            )
        })?;
        tracing::info!(pid = child.id(), "Mihomo core started");
        self.child = Some(child);
        Ok(())
    }

    pub fn stop(&mut self) -> eyre::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if child.try_wait()?.is_none() {
            child.kill()?;
        }
        let status = child.wait()?;
        tracing::info!(?status, "Mihomo core stopped");
        Ok(())
    }

    pub fn try_wait(&mut self) -> eyre::Result<Option<ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child.try_wait()?;
        if status.is_some() {
            self.child = None;
        }
        Ok(status)
    }
}

impl Drop for SidecarManager {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::error!(%error, "Failed to stop Mihomo core while shutting down");
        }
    }
}

fn path_requires_existence_check(path: &Path) -> bool {
    path.is_absolute() || path.components().count() > 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_missing_working_directory_before_spawning() {
        let mut sidecar = SidecarManager::new(
            PathBuf::from("mihomo"),
            std::env::temp_dir().join("slint-clash-missing-directory"),
            PathBuf::from("config.yaml"),
        );
        assert!(sidecar.start().is_err());
    }
}
