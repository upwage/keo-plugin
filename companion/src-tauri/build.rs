// The app manifest lists the app's own commands, so a capability must grant each one by name
// (capabilities/panel.json); a command missing from this list is not callable from any window.
const COMMANDS: &[&str] = &[
    "get_status",
    "sign_in",
    "sign_out",
    "set_paused",
    "set_start_at_login",
    "add_dir",
    "remove_dir",
    "log_tail",
    "open_url",
    "quit",
    "cancel_sign_in",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("tauri build step failed");
}
