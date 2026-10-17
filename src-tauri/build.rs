// Declaring the app's commands makes Tauri generate one permission per command
// (`allow-preview` and so on). A window can then call a command only if a file
// in capabilities/ grants it, instead of every window being able to call all.
const COMMANDS: &[&str] = &[
    "get_status",
    "get_settings",
    "save_settings",
    "preview",
    "run_selected",
    "get_history",
    "get_launch_at_login",
    "set_launch_at_login",
    "pick_folder",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
