//! BitGalaxy Shell AI 基座（Tauri 2，P6 后台服务化）：
//! - 后台服务进程：WS 服务（/ws :7777）只承载 AI / 审批 / 设置 / 主机档案 / Agent 帧；
//!   SSH/SFTP/串口/SQL 数据面归三工具壳（bgshell-ssh/sql/serial，7771/7772/7773）
//! - 无主工作台窗口：可见形态仅系统托盘 + 桌宠窗（Cargo feature `pet`，桌宠即 AI 形象）
//! - 托盘：拉起三工具 EXE / 桌宠开关（完整版）/ 退出；不注册开机自启——
//!   基座由任一工具的 AI 侧栏按需静默拉起（spawn_ai_base，见 bg-core::ai_base）
//! - MCP 子模式（--mcp-stdio）保持不变

use std::path::PathBuf;
use std::sync::Arc;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use bg_core::protocol::{AgentOp, AgentResult};
use bg_core::stores::Stores;

pub mod server;
#[cfg(feature = "pet")]
pub mod pet_pack;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 宠物窗尺寸（逻辑像素，桌宠精灵素材 256 + 气泡余量）；仅完整版存在宠物窗
#[cfg(feature = "pet")]
const PET_W: f64 = 340.0;
#[cfg(feature = "pet")]
const PET_H: f64 = 420.0;

/// 数据目录：Windows %APPDATA%\bitgalaxy-shell；其余平台 ~/.local/state/bitgalaxy-shell
pub fn resolve_data_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("bitgalaxy-shell");
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".local").join("state").join("bitgalaxy-shell")
}

/// 日志：控制台（GUI 子系统下可能无效）+ 数据目录文件（滚动追加）
pub fn append_log(data_dir: &PathBuf, msg: &str) {
    let line = format!("{} {msg}\n", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("agent.log"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

// ── Tauri IPC 命令（桌宠窗使用）──────────────────────────────────────────

/// 托盘"桌宠开关"菜单项句柄：隐藏/唤出后同步文案（隐藏桌宠 ⇄ 显示桌宠）
struct PetToggle(MenuItem<tauri::Wry>);

/// 按宠物窗当前可见性同步托盘菜单文案
fn sync_pet_tray_text(app: &tauri::AppHandle) {
    let visible = app
        .get_webview_window("pet")
        .map(|w| w.is_visible().unwrap_or(true))
        .unwrap_or(true);
    if let Some(toggle) = app.try_state::<PetToggle>() {
        let _ = toggle.0.set_text(if visible { "隐藏桌宠" } else { "显示桌宠" });
    }
}

/// 拉起三工具应用（桌宠菜单 / 前端兜底页入口）：同目录 spawn 兄弟 EXE。
/// tool ∈ ssh|sql|serial；工具壳自带 single-instance（已运行则唤起其主窗）
#[tauri::command]
fn launch_tool(tool: String) -> Result<(), String> {
    let exe = match tool.as_str() {
        "ssh" => "bgshell-ssh.exe",
        "sql" => "bgshell-sql.exe",
        "serial" => "bgshell-serial.exe",
        other => return Err(format!("未知工具：{other}")),
    };
    bg_core::ai_base::spawn_sibling_exe(exe)
}

/// 内置动作包（鲸鱼娘）落盘根目录（P6.5 桌宠资产外置：bundle.resources/pet-packs）。
/// 前端经 asset protocol 加载帧素材；目录不存在（未随包安装，桌宠可选安装）
/// 返回 Err → 前端降级为无精灵形态（聊天/菜单不受影响）。
/// 布局：资源按 slice glob 原样映射 → <资源根>/resources/pet-packs/builtin-whale
/// （tauri-build 开发期拷到 target/{profile}/resources/…，NSIS 安装到安装目录 resources/…）
#[tauri::command]
fn builtin_pack_root(app: tauri::AppHandle) -> Result<String, String> {
    let join_rel = |mut p: std::path::PathBuf| {
        for c in ["resources", "pet-packs", "builtin-whale"] {
            p = p.join(c);
        }
        p
    };
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = app.path().resource_dir() {
        candidates.push(join_rel(rd));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(join_rel(dir.to_path_buf()));
        }
    }
    for dir in &candidates {
        if dir.is_dir() {
            return Ok(dir.to_string_lossy().into_owned());
        }
    }
    Err("内置动作包未安装（resources/pet-packs/builtin-whale 缺失）".into())
}

/// 隐藏/唤出桌宠（宠物窗菜单隐藏后，经托盘开关或二次启动唤回）；
/// 无宠物窗构建（窗口查找返回 None → no-op），但命令保留注册以匹配
/// build.rs 的应用 ACL 清单与 capabilities/default.json 授权
#[tauri::command]
fn set_pet_visible(app: tauri::AppHandle, visible: bool) {
    if let Some(win) = app.get_webview_window("pet") {
        let _ = if visible { win.show() } else { win.hide() };
        if visible {
            let _ = win.set_focus();
        }
    }
    sync_pet_tray_text(&app);
}

/// 退出整个应用（桌宠右键菜单「退出」，与托盘「退出」同路径）
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

/// Agent 自身控制（基座：版本信息 / 退出；P6 已去自启与 openOnStart）
fn make_agent_ctl(app: tauri::AppHandle, data_dir: PathBuf) -> server::AgentCtl {
    Arc::new(move |op: AgentOp| {
        let app = app.clone();
        let data_dir = data_dir.clone();
        Box::pin(async move {
            match op.op.as_str() {
                "get-info" => AgentResult {
                    op: "get-info".into(),
                    ok: true,
                    autostart: None,
                    open_on_start: None,
                    version: Some(VERSION.into()),
                    message: None,
                },
                "shutdown" => {
                    append_log(&data_dir, "[bgshell] 收到退出指令，基座进程退出");
                    let app = app.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        app.exit(0);
                    });
                    AgentResult {
                        op: "shutdown".into(),
                        ok: true,
                        autostart: None,
                        open_on_start: None,
                        version: None,
                        message: None,
                    }
                }
                other => AgentResult {
                    op: other.into(),
                    ok: false,
                    autostart: None,
                    open_on_start: None,
                    version: None,
                    message: Some("未知操作".into()),
                },
            }
        })
    })
}

/// 注册 IPC 命令：动作包管理命令依赖 zip 且仅桌宠场景使用，只在完整版（feature = "pet"）编译
#[cfg(feature = "pet")]
fn with_ipc_commands(b: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    b.invoke_handler(tauri::generate_handler![
        launch_tool,
        builtin_pack_root,
        set_pet_visible,
        quit_app,
        pet_pack::pet_pack_import,
        pet_pack::pet_packs_list,
        pet_pack::pet_pack_delete,
        pet_pack::pet_pack_set_enabled
    ])
}

#[cfg(not(feature = "pet"))]
fn with_ipc_commands(b: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    b.invoke_handler(tauri::generate_handler![launch_tool, builtin_pack_root, set_pet_visible, quit_app])
}

/// dev server 探测：IPv4/IPv6 双栈 + HTTP 层验证（TCP 通且有 HTTP 响应才算就绪；
/// vite 关闭或仅剩半死连接时回退随包资产，避免窗口锁定已死的 devUrl）
fn dev_server_ready(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::time::Duration;
    let addrs = [
        std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::net::SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
    ];
    for addr in addrs {
        let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)) else {
            continue;
        };
        let req = format!("GET / HTTP/1.0\r\nHost: localhost:{port}\r\n\r\n");
        if stream.write_all(req.as_bytes()).is_err() {
            continue;
        }
        let mut buf = [0u8; 16];
        if matches!(stream.read(&mut buf), Ok(n) if n > 0) {
            return true;
        }
    }
    false
}

pub fn run() {
    let builder = tauri::Builder::default()
        // 桌面单实例：二次启动唤回宠物窗（隐藏时也能找回）；无宠物窗构建无窗口可唤。
        // --silent（工具壳 spawn_ai_base 在基座已在线时的竞态兜底）不唤起任何窗口
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if args.iter().any(|a| a == "--silent") {
                return;
            }
            #[cfg(feature = "pet")]
            {
                if let Some(win) = app.get_webview_window("pet") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
                sync_pet_tray_text(app);
            }
        }))
        // 桌宠对话复制：原生剪贴板读写（经 plugin:clipboard-manager IPC）
        .plugin(tauri_plugin_clipboard_manager::init())
        // 原生文件选择器（桌宠动作包 zip 导入）
        .plugin(tauri_plugin_dialog::init());
    let app = with_ipc_commands(builder)
        .setup(move |app| {
            let data_dir = resolve_data_dir();
            append_log(&data_dir, &format!("[bgshell] v{VERSION} 启动（AI 基座·后台服务）"));

            // ── 持久层（P6.5 按域拆库：hosts/settings/chat + 旧库迁移）+ WS 服务 ──
            let stores = Stores::open_domains(&data_dir);

            let agent_ctl = make_agent_ctl(app.handle().clone(), data_dir.clone());
            let policy = bg_core::policy::load_ai_policy(&data_dir, &stores);
            let port: u16 = std::env::var("BGSH_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(7777);
            let dispatcher = std::sync::Arc::new(server::DesktopDispatcher::new(
                stores,
                policy,
                agent_ctl,
            ));
            tauri::async_runtime::spawn(async move {
                if let Err(e) = bg_server::WsServer::start(port, dispatcher, None).await {
                    eprintln!("[bgshell] WS 服务启动失败: {e}");
                }
            });

            // ── 系统托盘：拉起三工具 / 桌宠开关（完整版）/ 退出 ──────────
            let tool_ssh = MenuItem::with_id(app, "tool-ssh", "打开 SSH 终端", true, None::<&str>)?;
            let tool_sql = MenuItem::with_id(app, "tool-sql", "打开 SQL 客户端", true, None::<&str>)?;
            let tool_serial = MenuItem::with_id(app, "tool-serial", "打开串口工具", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            #[cfg(feature = "pet")]
            let menu = {
                let pet_toggle = MenuItem::with_id(app, "pet", "隐藏桌宠", true, None::<&str>)?;
                app.manage(PetToggle(pet_toggle.clone()));
                Menu::with_items(app, &[&tool_ssh, &tool_sql, &tool_serial, &pet_toggle, &quit])?
            };
            #[cfg(not(feature = "pet"))]
            let menu = Menu::with_items(app, &[&tool_ssh, &tool_sql, &tool_serial, &quit])?;
            TrayIconBuilder::with_id("tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("BitGalaxy Shell · AI 基座")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    // 拉起三工具 EXE（同目录 spawn；工具壳 single-instance 自行唤起主窗）
                    "tool-ssh" => log_spawn("bgshell-ssh.exe"),
                    "tool-sql" => log_spawn("bgshell-sql.exe"),
                    "tool-serial" => log_spawn("bgshell-serial.exe"),
                    // 桌宠开关：按当前可见性取反（隐藏 ⇄ 唤出）；无宠物窗构建无此菜单项
                    "pet" => {
                        #[cfg(feature = "pet")]
                        {
                            if let Some(win) = app.get_webview_window("pet") {
                                let show = !win.is_visible().unwrap_or(true);
                                let _ = if show { win.show() } else { win.hide() };
                                if show {
                                    let _ = win.set_focus();
                                }
                            }
                            sync_pet_tray_text(app);
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // ── 鲸鱼娘桌宠窗（仅完整版）：透明置顶 · 无边框 · 不进任务栏 ─────
            // 基座唯一可见窗口 = AI 的形象与对话面板（dev 加载 vite MPA /pet/ 入口）
            #[cfg(feature = "pet")]
            {
                let pet_url = if cfg!(debug_assertions) && dev_server_ready(17177) {
                    WebviewUrl::External("http://localhost:17177/pet/".parse().unwrap())
                } else {
                    WebviewUrl::App("pet/index.html".into())
                };
                let mut pet = WebviewWindowBuilder::new(app, "pet", pet_url)
                    .title("鲸鱼娘桌宠")
                    // TODO(debug): 临时开 CDP 9222 定位宠物窗 IPC——排查完删除
                    .additional_browser_args(
                        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port=9222",
                    )
                    .inner_size(PET_W, PET_H)
                    .decorations(false)
                    .transparent(true)
                    .shadow(false)
                    .always_on_top(true)
                    .skip_taskbar(true)
                    .resizable(false)
                    .focused(false);
                // 主显示器右下角（逻辑坐标），预留任务栏高度
                if let Ok(Some(monitor)) = app.primary_monitor() {
                    let scale = monitor.scale_factor();
                    let mw = monitor.size().width as f64 / scale;
                    let mh = monitor.size().height as f64 / scale;
                    let taskbar = if monitor.position().y >= 0 { 48.0 } else { 0.0 };
                    pet = pet.position(mw - PET_W - 24.0, mh - PET_H - 24.0 - taskbar);
                }
                pet.build()?;
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("初始化 Tauri 失败");

    // 基座常驻事件循环：退出走托盘/桌宠菜单
    app.run(|_app, _event| {})
}

/// 托盘拉起工具 EXE：失败写基座日志（agent.log），不打断菜单事件循环
fn log_spawn(exe: &str) {
    let data_dir = resolve_data_dir();
    if let Err(e) = bg_core::ai_base::spawn_sibling_exe(exe) {
        append_log(&data_dir, &format!("[bgshell] 托盘拉起 {exe} 失败：{e}"));
    }
}
