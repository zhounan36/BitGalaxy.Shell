//! AI 基座与兄弟进程助手（四 EXE 架构）：
//! - 三工具壳（ssh/sql/serial）经 ensure_ai_base 按需静默拉起 AI 基座
//!   （bgshell-desktop.exe，WS 7777，BGSH_PORT 覆盖，--silent 后台常驻不弹窗）
//! - AI 基座经 spawn_sibling_exe 拉起三工具 EXE（托盘菜单 / 桌宠菜单入口）
//! 拉起前先探测端口：基座已在线时不再 spawn，避免二次实例触发
//! single-instance 回调唤起窗口（基座侧对 --silent 也做了抑制兜底）。

use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

/// AI 基座端口（与主程序一致；BGSH_PORT 环境变量覆盖）
pub fn ai_base_port() -> u16 {
    std::env::var("BGSH_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7777)
}

/// 端口是否已有监听（仅 127.0.0.1；WS 服务只绑回环，TCP 可连即视为在线）
pub fn port_listening(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

/// 基座 exe：与当前工具壳 exe 同目录（安装包四 EXE 同目录分发；dev 同在 target/{debug,release}）
fn ai_base_exe() -> Result<PathBuf, String> {
    let dir = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("无法定位当前 exe 所在目录")?
        .to_path_buf();
    let exe = dir.join("bgshell-desktop.exe");
    if exe.is_file() {
        Ok(exe)
    } else {
        Err(format!("未找到 AI 基座 bgshell-desktop.exe（{}）", exe.display()))
    }
}

/// 确保 AI 基座在线：已监听直接返回；否则静默拉起并等待 WS 端口就绪。
/// 返回 Ok(true)=本次拉起 / Ok(false)=已在线；Err=找不到 exe、spawn 失败或就绪超时。
/// 注意：含最长 15s 轮询等待，调用方应放在阻塞线程（如 spawn_blocking）。
pub fn ensure_ai_base() -> Result<bool, String> {
    let port = ai_base_port();
    if port_listening(port) {
        return Ok(false);
    }
    let exe = ai_base_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--silent");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：基座 --silent 下本就无控制台，双保险防闪窗
        cmd.creation_flags(0x0800_0000);
    }
    cmd.spawn().map_err(|e| format!("拉起 AI 基座失败：{e}"))?;
    // 基座 setup → WsServer::start 绑定成功通常 <2s；15s 兜底冷启动（WebView2 初始化）
    for _ in 0..50 {
        if port_listening(port) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Err("AI 基座已启动但端口 15s 内未就绪".into())
}

/// 拉起同目录的兄弟 EXE（基座托盘/桌宠菜单 → 三工具应用）。
/// exe_name 如 "bgshell-ssh.exe"；不存在/启动失败返回可读错误（弹托盘日志用）。
/// 工具壳自带 single-instance：已运行时二次 spawn 由其自行唤起主窗。
pub fn spawn_sibling_exe(exe_name: &str) -> Result<(), String> {
    let dir = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("无法定位当前 exe 所在目录")?
        .to_path_buf();
    let exe = dir.join(exe_name);
    if !exe.is_file() {
        return Err(format!("未找到 {exe_name}（{}）", exe.display()));
    }
    let mut cmd = std::process::Command::new(&exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.spawn().map_err(|e| format!("拉起 {exe_name} 失败：{e}"))?;
    Ok(())
}
