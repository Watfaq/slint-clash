// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Slint Clash — a native Mihomo GUI built with Slint and Material 3.

mod api;
mod config;
mod controller;
mod i18n;
mod log;
mod sidecar;

use tracing::level_filters::LevelFilter;

slint::include_modules!();

fn main() -> eyre::Result<()> {
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();
    i18n::init(&requested_languages);
    log::init(LevelFilter::DEBUG)?;

    let runtime = tokio::runtime::Runtime::new()?;
    let window = MainWindow::new()?;
    let controller = controller::Controller::bind(&window, runtime.handle().clone());

    let run_result = window.run();
    controller.shutdown();
    run_result?;
    Ok(())
}
