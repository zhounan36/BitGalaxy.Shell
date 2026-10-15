fn main() {
    // 应用自身命令默认仅本地上下文可用；dev 模式窗口加载 vite（远程上下文），
    // 必须生成应用 ACL 清单（__app-acl__）并在 capability 中显式授权，
    // 否则远程上下文调用会被 ACL 拒绝（quit_app / show_main_window 等全部失效）。
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
            "show_main_window",
            "dbg_evt",
            "set_pet_visible",
            "quit_app",
            "stat_file",
            "read_file_chunk",
            "write_text_file",
        ])),
    )
    .expect("failed to run tauri-build");
}
