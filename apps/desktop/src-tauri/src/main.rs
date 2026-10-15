// 防止 release 构建闪控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// 单进程入口：
//   --mcp-stdio：作为 AI 客户端的 MCP 子进程运行（stdout 专用协议，不初始化 GUI）
//   其余：桌面应用（桌宠 + 托盘 + 内嵌 Agent WS 服务）
fn main() {
    if std::env::args().any(|a| a == "--mcp-stdio") || std::env::var("BGSH_MCP").as_deref() == Ok("stdio") {
        let data_dir = bgshell_desktop_lib::resolve_data_dir();
        bg_ai::mcp::run_stdio(&data_dir);
        return;
    }
    bgshell_desktop_lib::run()
}
