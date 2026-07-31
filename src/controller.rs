// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use slint::ComponentHandle;
use tokio::runtime::Handle;

use crate::{api::ClashApi, config::Config, sidecar::SidecarManager, MainAdapter, MainWindow};

struct AppState {
    config: Config,
    sidecar: Option<SidecarManager>,
    api: Option<Arc<ClashApi>>,
    generation: u64,
    shutting_down: bool,
}

pub struct Controller {
    state: Arc<Mutex<AppState>>,
}

impl Controller {
    pub fn bind(window: &MainWindow, runtime: Handle) -> Self {
        let (config, load_error) = match Config::load() {
            Ok(config) => (config, None),
            Err(error) => (
                Config::default(),
                Some(format!("Could not load application settings: {error}")),
            ),
        };
        let state = Arc::new(Mutex::new(AppState {
            config,
            sidecar: None,
            api: None,
            generation: 0,
            shutting_down: false,
        }));

        initialize_ui(window, &state, load_error);
        bind_core_toggle(window, &state, &runtime);
        bind_mode_selection(window, &state, &runtime);
        bind_profile_actions(window, &state, &runtime);
        bind_setting_actions(window, &state);
        spawn_monitor(window.as_weak(), state.clone(), runtime);

        Self { state }
    }

    pub fn shutdown(&self) {
        let sidecar = {
            let mut state = lock(&self.state);
            state.shutting_down = true;
            state.generation = state.generation.wrapping_add(1);
            state.api = None;
            state.sidecar.take()
        };
        if let Some(mut sidecar) = sidecar {
            if let Err(error) = sidecar.stop() {
                tracing::error!(%error, "Failed to stop Mihomo during shutdown");
            }
        }
    }
}

fn initialize_ui(window: &MainWindow, state: &Arc<Mutex<AppState>>, load_error: Option<String>) {
    let adapter = window.global::<MainAdapter>();
    let config = lock(state).config.clone();
    adapter.set_setting_binary_path(config.clash_binary_path.clone().unwrap_or_default().into());
    adapter.set_setting_config_dir(config.config_dir.clone().unwrap_or_default().into());
    adapter.set_setting_api_port(config.api_port as i32);
    adapter.set_setting_api_port_text(config.api_port.to_string().into());
    adapter.set_setting_api_secret(config.api_secret.clone().unwrap_or_default().into());
    adapter.set_active_profile(config.active_profile.clone().unwrap_or_default().into());
    adapter.set_status_message("Core is stopped".into());

    if let Err(error) = scan_profiles_into(&config, &adapter) {
        adapter.set_error_message(error.to_string().into());
    } else if let Some(error) = load_error {
        adapter.set_error_message(error.into());
    }
}

fn bind_core_toggle(window: &MainWindow, state: &Arc<Mutex<AppState>>, runtime: &Handle) {
    let weak_window = window.as_weak();
    let shared_state = state.clone();
    let runtime = runtime.clone();
    window.global::<MainAdapter>().on_toggle_core(move || {
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let adapter = window.global::<MainAdapter>();
        if adapter.get_core_busy() {
            return;
        }

        if adapter.get_core_running() {
            stop_core(weak_window.clone(), shared_state.clone(), runtime.clone());
        } else {
            start_core(weak_window.clone(), shared_state.clone(), runtime.clone());
        }
    });
}

fn start_core(weak_window: slint::Weak<MainWindow>, state: Arc<Mutex<AppState>>, runtime: Handle) {
    let (config, generation) = {
        let mut state = lock(&state);
        if state.shutting_down || state.sidecar.is_some() {
            return;
        }
        state.generation = state.generation.wrapping_add(1);
        (state.config.clone(), state.generation)
    };

    if let Some(window) = weak_window.upgrade() {
        let adapter = window.global::<MainAdapter>();
        adapter.set_core_busy(true);
        adapter.set_error_message("".into());
        adapter.set_status_message("Starting Mihomo core…".into());
    }

    runtime.spawn(async move {
        let binary = config.clash_binary();
        let work_dir = config.config_dir();
        let config_path = config.active_config_path();
        let start_result = tokio::task::spawn_blocking(move || {
            let mut sidecar = SidecarManager::new(binary, work_dir, config_path);
            sidecar.start()?;
            Ok::<_, eyre::Report>(sidecar)
        })
        .await;

        let mut sidecar = match start_result {
            Ok(Ok(sidecar)) => sidecar,
            Ok(Err(error)) => {
                report_start_failure(weak_window, error.to_string());
                return;
            }
            Err(error) => {
                report_start_failure(
                    weak_window,
                    format!("Mihomo start task failed unexpectedly: {error}"),
                );
                return;
            }
        };

        let api = Arc::new(ClashApi::new(config.api_url(), config.api_secret.clone()));
        let mut last_error = None;
        let mut version = None;
        for _ in 0..20 {
            match api.version().await {
                Ok(value) => {
                    version = Some(value.version.unwrap_or_else(|| "unknown".to_owned()));
                    break;
                }
                Err(error) => last_error = Some(error.to_string()),
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        let Some(version) = version else {
            let _ = tokio::task::spawn_blocking(move || sidecar.stop()).await;
            report_start_failure(
                weak_window,
                format!(
                    "Mihomo started but its API did not become ready: {}",
                    last_error.unwrap_or_else(|| "unknown error".to_owned())
                ),
            );
            return;
        };
        let mode_index = api
            .runtime_config()
            .await
            .map(|config| mode_index(&config.mode))
            .unwrap_or(0);

        let mut sidecar = Some(sidecar);
        let accepted = {
            let mut state = lock(&state);
            if state.generation == generation && !state.shutting_down {
                state.sidecar = sidecar.take();
                state.api = Some(api);
                true
            } else {
                false
            }
        };

        if !accepted {
            if let Some(mut sidecar) = sidecar {
                let _ = tokio::task::spawn_blocking(move || sidecar.stop()).await;
            }
            return;
        }

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak_window.upgrade() {
                let adapter = window.global::<MainAdapter>();
                adapter.set_core_running(true);
                adapter.set_core_busy(false);
                adapter.set_clash_version(version.into());
                adapter.set_clash_mode_index(mode_index);
                adapter.set_status_message("Mihomo core is running".into());
            }
        });
    });
}

fn bind_mode_selection(window: &MainWindow, state: &Arc<Mutex<AppState>>, runtime: &Handle) {
    let weak_window = window.as_weak();
    let shared_state = state.clone();
    let runtime = runtime.clone();
    window
        .global::<MainAdapter>()
        .on_set_clash_mode(move |index| {
            let mode = match index {
                0 => "rule",
                1 => "global",
                2 => "direct",
                _ => {
                    set_error(weak_window.clone(), "Unknown proxy mode".to_owned());
                    return;
                }
            };
            let api = lock(&shared_state).api.clone();
            let Some(api) = api else {
                set_error(
                    weak_window.clone(),
                    "Start the Mihomo core before changing mode".to_owned(),
                );
                return;
            };

            set_busy_status(
                weak_window.clone(),
                true,
                format!("Switching to {mode} mode…"),
            );
            let weak_window = weak_window.clone();
            runtime.spawn(async move {
                let result = api.set_mode(mode).await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak_window.upgrade() {
                        let adapter = window.global::<MainAdapter>();
                        adapter.set_core_busy(false);
                        match result {
                            Ok(()) => {
                                adapter.set_clash_mode_index(index);
                                adapter.set_status_message(format!("Proxy mode: {mode}").into());
                                adapter.set_error_message("".into());
                            }
                            Err(error) => {
                                adapter.set_status_message("Mode switch failed".into());
                                adapter.set_error_message(error.to_string().into());
                            }
                        }
                    }
                });
            });
        });
}

fn report_start_failure(weak_window: slint::Weak<MainWindow>, message: String) {
    tracing::error!(%message, "Could not start Mihomo core");
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak_window.upgrade() {
            let adapter = window.global::<MainAdapter>();
            adapter.set_core_running(false);
            adapter.set_core_busy(false);
            adapter.set_status_message("Core failed to start".into());
            adapter.set_error_message(message.into());
        }
    });
}

fn stop_core(weak_window: slint::Weak<MainWindow>, state: Arc<Mutex<AppState>>, runtime: Handle) {
    let sidecar = {
        let mut state = lock(&state);
        state.generation = state.generation.wrapping_add(1);
        state.api = None;
        state.sidecar.take()
    };

    if let Some(window) = weak_window.upgrade() {
        let adapter = window.global::<MainAdapter>();
        adapter.set_core_busy(true);
        adapter.set_status_message("Stopping Mihomo core…".into());
    }

    runtime.spawn(async move {
        let result = if let Some(mut sidecar) = sidecar {
            tokio::task::spawn_blocking(move || sidecar.stop())
                .await
                .map_err(eyre::Report::from)
                .and_then(|result| result)
        } else {
            Ok(())
        };

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak_window.upgrade() {
                let adapter = window.global::<MainAdapter>();
                adapter.set_core_running(false);
                adapter.set_core_busy(false);
                adapter.set_clash_version("".into());
                adapter.set_traffic_upload("—".into());
                adapter.set_traffic_download("—".into());
                adapter.set_status_message("Core is stopped".into());
                if let Err(error) = result {
                    adapter.set_error_message(
                        format!("Could not stop Mihomo cleanly: {error}").into(),
                    );
                }
            }
        });
    });
}

fn bind_profile_actions(window: &MainWindow, state: &Arc<Mutex<AppState>>, runtime: &Handle) {
    let weak_window = window.as_weak();
    let shared_state = state.clone();
    let runtime_handle = runtime.clone();
    window
        .global::<MainAdapter>()
        .on_select_profile(move |profile| {
            let profile = profile.to_string();
            let (api, path) = {
                let state = lock(&shared_state);
                (state.api.clone(), state.config.profile_path(&profile))
            };
            let Some(path) = path else {
                set_error(
                    weak_window.clone(),
                    format!("Profile no longer exists: {profile}"),
                );
                return;
            };

            if let Some(api) = api {
                set_busy_status(weak_window.clone(), true, format!("Activating {profile}…"));
                let weak_window = weak_window.clone();
                let state = shared_state.clone();
                runtime_handle.spawn(async move {
                    let result = api.reload_config(&path.to_string_lossy()).await;
                    finish_profile_selection(weak_window, state, profile, result);
                });
            } else {
                finish_profile_selection(
                    weak_window.clone(),
                    shared_state.clone(),
                    profile,
                    Ok(()),
                );
            }
        });

    let weak_window = window.as_weak();
    let shared_state = state.clone();
    let runtime_handle = runtime.clone();
    window.global::<MainAdapter>().on_reload_config(move || {
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let adapter = window.global::<MainAdapter>();
        let (config, api) = {
            let state = lock(&shared_state);
            (state.config.clone(), state.api.clone())
        };
        if let Err(error) = scan_profiles_into(&config, &adapter) {
            adapter.set_error_message(error.to_string().into());
            return;
        }

        let Some(api) = api else {
            adapter.set_status_message("Profile list refreshed".into());
            return;
        };
        let path = config.active_config_path();
        set_busy_status(
            weak_window.clone(),
            true,
            "Reloading active profile…".to_owned(),
        );
        let weak_window = weak_window.clone();
        runtime_handle.spawn(async move {
            let result = api.reload_config(&path.to_string_lossy()).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak_window.upgrade() {
                    let adapter = window.global::<MainAdapter>();
                    adapter.set_core_busy(false);
                    match result {
                        Ok(()) => {
                            adapter.set_status_message("Active profile reloaded".into());
                        }
                        Err(error) => {
                            adapter.set_error_message(error.to_string().into());
                            adapter.set_status_message("Profile reload failed".into());
                        }
                    }
                }
            });
        });
    });
}

fn finish_profile_selection(
    weak_window: slint::Weak<MainWindow>,
    state: Arc<Mutex<AppState>>,
    profile: String,
    reload_result: eyre::Result<()>,
) {
    let result = reload_result.and_then(|()| {
        let config = {
            let mut state = lock(&state);
            state.config.active_profile = Some(profile.clone());
            state.config.clone()
        };
        config.save()
    });

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak_window.upgrade() {
            let adapter = window.global::<MainAdapter>();
            adapter.set_core_busy(false);
            match result {
                Ok(()) => {
                    adapter.set_active_profile(profile.clone().into());
                    adapter.set_status_message(format!("Profile “{profile}” is active").into());
                    adapter.set_error_message("".into());
                }
                Err(error) => {
                    adapter.set_status_message("Profile activation failed".into());
                    adapter.set_error_message(error.to_string().into());
                }
            }
        }
    });
}

fn bind_setting_actions(window: &MainWindow, state: &Arc<Mutex<AppState>>) {
    let weak_window = window.as_weak();
    let shared_state = state.clone();
    window.global::<MainAdapter>().on_start_edit(move |index| {
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let adapter = window.global::<MainAdapter>();
        let config = lock(&shared_state).config.clone();
        let value = match index {
            0 => config.clash_binary_path.unwrap_or_default(),
            1 => config.config_dir.unwrap_or_default(),
            2 => config.api_port.to_string(),
            3 => config.api_secret.unwrap_or_default(),
            _ => String::new(),
        };
        adapter.set_edit_value(value.into());
        adapter.set_editing_field_index(index);
        adapter.set_show_edit_ui(true);
    });

    let weak_window = window.as_weak();
    let shared_state = state.clone();
    window
        .global::<MainAdapter>()
        .on_save_setting(move |index, value| {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            let adapter = window.global::<MainAdapter>();
            let value = value.trim().to_owned();

            let update = update_setting(&shared_state, index, &value);
            let (config, restart_required) = match update {
                Ok(value) => value,
                Err(error) => {
                    adapter.set_error_message(error.to_string().into());
                    return;
                }
            };
            if let Err(error) = config.save() {
                adapter.set_error_message(format!("Could not save settings: {error}").into());
                return;
            }

            adapter.set_setting_binary_path(
                config.clash_binary_path.clone().unwrap_or_default().into(),
            );
            adapter.set_setting_config_dir(config.config_dir.clone().unwrap_or_default().into());
            adapter.set_setting_api_port(config.api_port as i32);
            adapter.set_setting_api_port_text(config.api_port.to_string().into());
            adapter.set_setting_api_secret(config.api_secret.clone().unwrap_or_default().into());
            adapter.set_show_edit_ui(false);
            adapter.set_editing_field_index(-1);
            adapter.set_error_message("".into());
            adapter.set_status_message(
                if restart_required {
                    "Setting saved; restart the core to apply it"
                } else {
                    "Setting saved"
                }
                .into(),
            );

            if index == 1 {
                if let Err(error) = scan_profiles_into(&config, &adapter) {
                    adapter.set_error_message(error.to_string().into());
                }
            }
        });

    let weak_window = window.as_weak();
    window.global::<MainAdapter>().on_cancel_edit(move || {
        if let Some(window) = weak_window.upgrade() {
            let adapter = window.global::<MainAdapter>();
            adapter.set_show_edit_ui(false);
            adapter.set_editing_field_index(-1);
            adapter.set_edit_value("".into());
        }
    });

    let weak_window = window.as_weak();
    window.global::<MainAdapter>().on_dismiss_error(move || {
        if let Some(window) = weak_window.upgrade() {
            window.global::<MainAdapter>().set_error_message("".into());
        }
    });
}

fn update_setting(
    state: &Arc<Mutex<AppState>>,
    index: i32,
    value: &str,
) -> eyre::Result<(Config, bool)> {
    let mut state = lock(state);
    let core_running = state.sidecar.is_some();
    match index {
        0 => {
            if !value.is_empty() && path_requires_file(value) && !Path::new(value).is_file() {
                return Err(eyre::eyre!("Mihomo binary does not exist: {value}"));
            }
            state.config.clash_binary_path = (!value.is_empty()).then(|| value.to_owned());
        }
        1 => {
            if !value.is_empty() && !Path::new(value).is_dir() {
                return Err(eyre::eyre!(
                    "Configuration directory does not exist: {value}"
                ));
            }
            state.config.config_dir = (!value.is_empty()).then(|| value.to_owned());
        }
        2 => {
            let port = value
                .parse::<u16>()
                .map_err(|_| eyre::eyre!("API port must be between 1 and 65535"))?;
            if port == 0 {
                return Err(eyre::eyre!("API port must be between 1 and 65535"));
            }
            state.config.api_port = port;
        }
        3 => state.config.api_secret = (!value.is_empty()).then(|| value.to_owned()),
        _ => return Err(eyre::eyre!("Unknown setting field")),
    }
    Ok((state.config.clone(), core_running && matches!(index, 0..=3)))
}

fn spawn_monitor(
    weak_window: slint::Weak<MainWindow>,
    state: Arc<Mutex<AppState>>,
    runtime: Handle,
) {
    runtime.spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(2));
        let mut traffic_failures = 0_u8;
        loop {
            timer.tick().await;

            let (api, exited, shutting_down) = {
                let mut state = lock(&state);
                let exited = state
                    .sidecar
                    .as_mut()
                    .and_then(|sidecar| match sidecar.try_wait() {
                        Ok(status) => status,
                        Err(error) => {
                            tracing::error!(%error, "Could not inspect Mihomo process");
                            None
                        }
                    });
                if exited.is_some() {
                    state.sidecar = None;
                    state.api = None;
                    state.generation = state.generation.wrapping_add(1);
                }
                (state.api.clone(), exited, state.shutting_down)
            };

            if shutting_down {
                break;
            }
            if let Some(status) = exited {
                let message = format!("Mihomo exited unexpectedly ({status})");
                let weak_window = weak_window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak_window.upgrade() {
                        let adapter = window.global::<MainAdapter>();
                        adapter.set_core_running(false);
                        adapter.set_core_busy(false);
                        adapter.set_status_message("Core stopped unexpectedly".into());
                        adapter.set_error_message(message.into());
                        adapter.set_traffic_upload("—".into());
                        adapter.set_traffic_download("—".into());
                    }
                });
                continue;
            }

            let Some(api) = api else {
                traffic_failures = 0;
                continue;
            };
            match api.traffic_sample().await {
                Ok(traffic) => {
                    traffic_failures = 0;
                    let weak_window = weak_window.clone();
                    let upload = format_rate(traffic.up);
                    let download = format_rate(traffic.down);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = weak_window.upgrade() {
                            let adapter = window.global::<MainAdapter>();
                            adapter.set_traffic_upload(upload.into());
                            adapter.set_traffic_download(download.into());
                            adapter.set_traffic_pulse(!adapter.get_traffic_pulse());
                        }
                    });
                }
                Err(error) => {
                    traffic_failures = traffic_failures.saturating_add(1);
                    tracing::warn!(%error, traffic_failures, "Could not sample Mihomo traffic");
                    if traffic_failures == 3 {
                        set_error(
                            weak_window.clone(),
                            format!("Live traffic is unavailable: {error}"),
                        );
                    }
                }
            }
        }
    });
}

fn scan_profiles_into(config: &Config, adapter: &MainAdapter) -> eyre::Result<()> {
    let directory = config.config_dir();
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            adapter.set_profiles(
                std::rc::Rc::new(slint::VecModel::<slint::SharedString>::default()).into(),
            );
            adapter.set_profiles_loaded(true);
            return Ok(());
        }
        Err(error) => {
            adapter.set_profiles(
                std::rc::Rc::new(slint::VecModel::<slint::SharedString>::default()).into(),
            );
            adapter.set_profiles_loaded(true);
            return Err(eyre::eyre!(
                "Could not read configuration directory '{}': {error}",
                directory.display()
            ));
        }
    };

    let mut profiles = entries
        .filter_map(Result::ok)
        .filter_map(|entry| profile_name(entry.path()))
        .collect::<Vec<_>>();
    profiles.sort_by_key(|name| name.to_lowercase());
    profiles.dedup();
    let profiles = profiles
        .into_iter()
        .map(slint::SharedString::from)
        .collect::<Vec<_>>();
    adapter.set_profiles(std::rc::Rc::new(slint::VecModel::from(profiles)).into());
    adapter.set_profiles_loaded(true);
    Ok(())
}

fn profile_name(path: PathBuf) -> Option<String> {
    let extension = path.extension()?.to_str()?;
    if !matches!(extension, "yaml" | "yml") {
        return None;
    }
    path.file_stem()?.to_str().map(ToOwned::to_owned)
}

fn set_error(weak_window: slint::Weak<MainWindow>, message: String) {
    tracing::error!(%message);
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak_window.upgrade() {
            window
                .global::<MainAdapter>()
                .set_error_message(message.into());
        }
    });
}

fn set_busy_status(weak_window: slint::Weak<MainWindow>, busy: bool, status: String) {
    if let Some(window) = weak_window.upgrade() {
        let adapter = window.global::<MainAdapter>();
        adapter.set_core_busy(busy);
        adapter.set_status_message(status.into());
    }
}

fn path_requires_file(value: &str) -> bool {
    let path = Path::new(value);
    path.is_absolute() || path.components().count() > 1
}

fn mode_index(mode: &str) -> i32 {
    match mode.to_ascii_lowercase().as_str() {
        "global" => 1,
        "direct" => 2,
        _ => 0,
    }
}

fn format_rate(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes < KIB {
        format!("{bytes:.0} B/s")
    } else if bytes < MIB {
        format!("{:.1} KiB/s", bytes / KIB)
    } else if bytes < GIB {
        format!("{:.2} MiB/s", bytes / MIB)
    } else {
        format!("{:.2} GiB/s", bytes / GIB)
    }
}

fn lock(state: &Arc<Mutex<AppState>>) -> MutexGuard<'_, AppState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_transfer_rates_without_mislabeling_bytes() {
        assert_eq!(format_rate(0), "0 B/s");
        assert_eq!(format_rate(512), "512 B/s");
        assert_eq!(format_rate(1024), "1.0 KiB/s");
        assert_eq!(format_rate(1_048_576), "1.00 MiB/s");
    }

    #[test]
    fn recognizes_only_yaml_profile_files() {
        assert_eq!(
            profile_name(PathBuf::from("example.yaml")),
            Some("example".to_owned())
        );
        assert_eq!(profile_name(PathBuf::from("example.txt")), None);
    }

    #[test]
    fn maps_runtime_modes_to_the_ui() {
        assert_eq!(mode_index("rule"), 0);
        assert_eq!(mode_index("GLOBAL"), 1);
        assert_eq!(mode_index("direct"), 2);
    }
}
