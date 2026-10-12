// 防止 release 构建闪控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    bgshell_desktop_lib::run()
}
