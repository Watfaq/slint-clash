// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Slint Clash — Clash GUI with Slint + Material 3
//
// Re-implementation of cosmic-clash using Slint Material 3 components.

mod api;
mod config;
mod i18n;
mod log;
mod sidecar;

use std::sync::{Arc, Mutex};

use api::ClashApi;
use config::Config;
use sidecar::SidecarManager;
use slint::ComponentHandle;
use tracing::level_filters::LevelFilter;

slint::include_modules!();

struct AppState {
    config: Config,
    sidecar: Option<SidecarManager>,
    api: Option<Arc<ClashApi>>,
}

fn main() -> eyre::Result<()> {
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();
    i18n::init(&requested_languages);
    log::init(LevelFilter::DEBUG)?;

    let rt = tokio::runtime::Runtime::new()?;

    let state = Arc::new(Mutex::new(AppState {
        config: Config::load().unwrap_or_default(),
        sidecar: None,
        api: None,
    }));

    let window = MainWindow::new()?;
    let adapter = window.global::<MainAdapter>();

    // Load initial config & scan profiles
    {
        let s = state.lock().unwrap();
        adapter.set_setting_binary_path(
            s.config
                .clash_binary_path
                .clone()
                .unwrap_or_default()
                .into(),
        );
        adapter.set_setting_config_dir(s.config.config_dir.clone().unwrap_or_default().into());
        adapter.set_setting_api_port(s.config.api_port as i32);
        adapter.set_setting_api_port_text(s.config.api_port.to_string().into());
        adapter.set_setting_api_secret(s.config.api_secret.clone().unwrap_or_default().into());
        adapter.set_active_profile(s.config.active_profile.clone().unwrap_or_default().into());
        scan_profiles_into(s.config.config_dir(), &adapter);
    }

    // ── Toggle VPN ──
    {
        let state = state.clone();
        let win = window.as_weak();
        let rt_handle = rt.handle().clone();
        adapter.on_toggle_vpn(move || {
            let win = win.unwrap();
            let a = win.global::<MainAdapter>();
            let mut s = state.lock().unwrap();

            if a.get_vpn_active() {
                if let Some(ref mut sc) = s.sidecar {
                    let _ = sc.stop();
                }
                a.set_vpn_active(false);
                a.set_clash_version("".into());
                a.set_traffic_upload("—".into());
                a.set_traffic_download("—".into());
                s.api = None;
            } else {
                let binary = s.config.clash_binary();
                let work_dir = s.config.config_dir();
                let config_path = work_dir.join("config.yaml");

                let mut sc = SidecarManager::new(binary, work_dir, config_path);
                if sc.start().is_ok() {
                    let api = Arc::new(ClashApi::new(
                        s.config.api_url(),
                        s.config.api_secret.clone(),
                    ));
                    s.api = Some(api.clone());
                    s.sidecar = Some(sc);
                    a.set_vpn_active(true);

                    let win_weak = win.as_weak();
                    rt_handle.spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        if let Ok(ver) = api.version().await {
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = win_weak.upgrade() {
                                    w.global::<MainAdapter>()
                                        .set_clash_version(ver.version.unwrap_or_default().into());
                                }
                            });
                        }
                    });
                }
            }
        });
    }

    // ── Select Profile ──
    {
        let state = state.clone();
        let win = window.as_weak();
        adapter.on_select_profile(move |profile| {
            let win = win.unwrap();
            let mut s = state.lock().unwrap();
            let p = profile.to_string();
            s.config.active_profile = Some(p.clone());
            let _ = s.config.save();
            win.global::<MainAdapter>()
                .set_active_profile(p.clone().into());

            if let Some(ref api) = s.api {
                let api = api.clone();
                let config_dir = s.config.config_dir();
                let path = config_dir
                    .join(format!("{}.yaml", p))
                    .to_string_lossy()
                    .to_string();
                tokio::spawn(async move {
                    let _ = api.reload_config(&path).await;
                });
            }
        });
    }

    // ── Reload Config ──
    {
        let state = state.clone();
        let win = window.as_weak();
        adapter.on_reload_config(move || {
            let win = win.unwrap();
            let a = win.global::<MainAdapter>();
            let s = state.lock().unwrap();

            if let Some(ref api) = s.api {
                let api = api.clone();
                let path = s.config.active_profile.clone().unwrap_or_else(|| {
                    s.config
                        .config_dir()
                        .join("config.yaml")
                        .to_string_lossy()
                        .to_string()
                });
                tokio::spawn(async move {
                    let _ = api.reload_config(&path).await;
                });
            }
            scan_profiles_into(s.config.config_dir(), &a);
        });
    }

    // ── Start Edit ──
    {
        let state = state.clone();
        let win = window.as_weak();
        adapter.on_start_edit(move |idx| {
            let win = win.unwrap();
            let a = win.global::<MainAdapter>();
            let s = state.lock().unwrap();

            let val: String = match idx {
                0 => s.config.clash_binary_path.clone().unwrap_or_default(),
                1 => s.config.config_dir.clone().unwrap_or_default(),
                2 => s.config.api_port.to_string(),
                3 => s.config.api_secret.clone().unwrap_or_default(),
                _ => String::new(),
            };
            a.set_edit_value(val.into());
            a.set_editing_field_index(idx);
            a.set_show_edit_ui(true);
        });
    }

    // ── Save Setting ──
    {
        let state = state.clone();
        let win = window.as_weak();
        adapter.on_save_setting(move |idx, value| {
            let win = win.unwrap();
            let a = win.global::<MainAdapter>();
            let mut s = state.lock().unwrap();
            let v = value.to_string();

            match idx {
                0 => {
                    s.config.clash_binary_path = if v.is_empty() { None } else { Some(v.clone()) };
                    a.set_setting_binary_path(v.into());
                }
                1 => {
                    s.config.config_dir = if v.is_empty() { None } else { Some(v.clone()) };
                    a.set_setting_config_dir(v.into());
                }
                2 => {
                    if let Ok(p) = v.parse::<u16>() {
                        s.config.api_port = p;
                        a.set_setting_api_port(p as i32);
                        a.set_setting_api_port_text(p.to_string().into());
                    }
                }
                3 => {
                    s.config.api_secret = if v.is_empty() { None } else { Some(v.clone()) };
                    a.set_setting_api_secret(v.into());
                }
                _ => {}
            }
            let _ = s.config.save();
            a.set_show_edit_ui(false);
            a.set_editing_field_index(-1);
        });
    }

    // ── Cancel Edit ──
    {
        let win = window.as_weak();
        adapter.on_cancel_edit(move || {
            if let Some(w) = win.upgrade() {
                let a = w.global::<MainAdapter>();
                a.set_show_edit_ui(false);
                a.set_editing_field_index(-1);
                a.set_edit_value("".into());
            }
        });
    }

    // ── Background traffic update task ──
    {
        let state = state.clone();
        let win = window.as_weak();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                let api_opt = {
                    let s = state.lock().unwrap();
                    s.api.clone()
                };

                if let Some(api) = api_opt {
                    if let Ok(t) = api.traffic().await {
                        let win = win.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = win.upgrade() {
                                let a = w.global::<MainAdapter>();
                                a.set_traffic_upload(format_bytes(t.up).into());
                                a.set_traffic_download(format_bytes(t.down).into());
                                a.set_traffic_pulse(!a.get_traffic_pulse());
                            }
                        });
                    }
                }
            }
        });
    }

    window.run()?;
    Ok(())
}

fn scan_profiles_into(dir: std::path::PathBuf, a: &MainAdapter) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut profiles: Vec<slint::SharedString> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .map_or(false, |e| e == "yaml" || e == "yml")
            {
                if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                    profiles.push(name.into());
                }
            }
        }
        a.set_profiles(std::rc::Rc::new(slint::VecModel::from(profiles)).into());
        a.set_profiles_loaded(true);
    }
}

fn format_bytes(bytes: u64) -> String {
    let kb = bytes as f64 / 1024.0;
    if kb < 1024.0 {
        format!("{:.1} KB/s", kb)
    } else {
        format!("{:.2} MB/s", kb / 1024.0)
    }
}
