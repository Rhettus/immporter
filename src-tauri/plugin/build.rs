// the plugin's commands, from which tauri-plugin generates the allow-*
// permissions apps grant in their capabilities
const COMMANDS: &[&str] = &[
    "dev_autoload_source",
    "list_volumes",
    "list_folders",
    "save_video_thumb",
    "scan_source",
    "close_source",
    "set_visible",
    "get_settings",
    "save_settings",
    "set_dedupe_config",
    "rebuild_dest_index",
    "example_path",
    "get_immich_settings",
    "save_immich_settings",
    "forget_immich_key",
    "test_immich",
    "start_import",
    "cancel_import",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}
