mod about;
pub mod cli;
mod commands;
mod config;
mod disk;
mod env;
mod history;
mod monitor;
mod poll;
mod proc;
mod runner;
mod settings;
mod state;
mod tasks;
mod tray;
mod updater;
mod volume;
mod welcome;
mod windows;

use std::sync::Arc;

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
        // Used only from Rust (updater.rs); its JS commands are not granted.
        // The endpoint and public key come from tauri.conf.json.
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Used only from Rust (commands::pick_folder); its JS commands are not granted.
        .plugin(tauri_plugin_dialog::init())
        // Off until the user turns it on in Settings.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::get_settings,
            commands::get_appearance,
            commands::save_settings,
            commands::preview,
            commands::run_selected,
            commands::get_history,
            commands::get_totals,
            commands::get_launch_at_login,
            commands::set_launch_at_login,
            commands::pick_folder,
            commands::get_ignored,
            commands::set_ignored,
            commands::get_protection,
            commands::get_not_activity,
            commands::check_for_update,
            commands::install_update,
            commands::restart_to_update,
            commands::open_link,
        ])
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
            // Without the button, alerts still work; only the shortcut to Clean is lost.
            if let Err(e) = poll::register_clean_action(app.handle()) {
                log::error!("{e:#}");
            }

            // Capacity 16 is plenty: the loop drains the queue on every wake.
            let (tx, rx) = mpsc::channel(16);
            let handle = app.handle().clone();
            let on_history = move || {
                if let Err(e) = windows::history_changed(&handle) {
                    log::warn!("could not tell the window that history changed: {e}");
                }
            };
            let updates = Arc::new(updater::Updates::new(tx.downgrade()));
            app.manage(Arc::clone(&updates));
            match poll::start_watchers(&config_path, tx, on_history) {
                // Managed state keeps the watcher alive for the life of the app.
                Ok(watcher) => {
                    app.manage(watcher);
                }
                Err(e) => log::error!(
                    "live reload, drive detection and the Clean tab's history refresh are off: {e:#}"
                ),
            }

            let shared = Arc::new(state::Shared::new(home, config_path));
            app.manage(Arc::clone(&shared));
            let poller = poll::Poller::new(app.handle().clone(), tray, Arc::clone(&shared));
            // Nothing that cleans or updates runs until the first-run warning is
            // accepted. Quit in the dialog exits before any of it starts.
            let accepted = welcome::confirm(app.handle(), shared.paths.warning_accepted.clone());
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match accepted.await {
                    Ok(true) => {}
                    Ok(false) => {
                        log::info!("the first-run warning was declined; quitting");
                        handle.exit(0);
                        return;
                    }
                    Err(_) => {
                        log::error!("the first-run warning closed without an answer; quitting");
                        handle.exit(1);
                        return;
                    }
                }
                tauri::async_runtime::spawn(poller.run(rx));
                tauri::async_runtime::spawn(updater::run_in_background(handle, shared, updates));
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Picard")
        .run(|_app, event| {
            // Tauri quits when the last window closes, which for a menu bar app
            // means closing the Picard window would quit it. Such exits carry no
            // code; Quit in the tray menu calls `app.exit(0)` and still works.
            if let tauri::RunEvent::ExitRequested {
                code: None, api, ..
            } = event
            {
                api.prevent_exit();
            }
        });
}
