pub mod cli;
mod config;
mod disk;
mod history;
mod monitor;
mod poll;
mod runner;
mod tasks;
mod tray;
mod volume;

use tauri::Manager;
use tokio::sync::mpsc;

/// Starts Picard as a menu bar app: no Dock icon and no window, only the tray.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let home = app.path().home_dir()?;
            let config_path = config::config_path(&home);
            // A failure here still lets Picard start; the menu shows the config error.
            match poll::ensure_config_file(&config_path) {
                Ok(true) => log::info!("wrote default config to {}", config_path.display()),
                Ok(false) => {}
                Err(e) => log::error!("{e:#}"),
            }

            let tray = tray::Tray::install(app.handle())?;

            // Capacity 16 is plenty: the loop drains the queue on every wake.
            let (tx, rx) = mpsc::channel(16);
            match poll::start_watchers(&config_path, tx) {
                // Managed state keeps the watcher alive for the life of the app.
                Ok(watcher) => {
                    app.manage(watcher);
                }
                Err(e) => log::error!("live reload and drive detection are off: {e:#}"),
            }

            let poller = poll::Poller::new(app.handle().clone(), tray, home, config_path);
            tauri::async_runtime::spawn(poller.run(rx));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Picard");
}
