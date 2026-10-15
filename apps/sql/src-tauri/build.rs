fn main() {
    // dev 模式窗口加载 vite（远程上下文），必须生成应用 ACL 清单并显式授权
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&["spawn_ai_base"])),
    )
    .expect("failed to run tauri-build");
}
