fn main() {
    // 应用自身命令默认仅本地上下文可用；dev 模式窗口加载 vite（远程上下文），
    // 必须生成应用 ACL 清单（__app-acl__）并在 capability 中显式授权，
    // 否则远程上下文调用会被 ACL 拒绝（launch_tool / quit_app 等全部失效）。
    // 动作包命令（pet_pack_*）在 pet feature 下由 pet_pack.rs 自带权限文件注册。
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
            "launch_tool",
            "builtin_pack_root",
            "set_pet_visible",
            "quit_app",
        ])),
    )
    .expect("failed to run tauri-build");
}
