mod disk;
mod tray;

use tauri::Manager;

/// Starts Picard as a menu bar app: no Dock icon and no window, only the tray.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let home = app.path().home_dir()?;
            tray::install(app.handle(), home)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Picard");
}
