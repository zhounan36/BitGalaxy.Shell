fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&["spawn_ai_base", "write_text_file"]),
        ),
    )
    .expect("failed to run tauri-build");
}
