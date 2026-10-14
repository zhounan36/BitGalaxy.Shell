//! BitGalaxy Shell 桌面应用（Tauri 2，单进程架构）：
//! - Agent 全内嵌：WS 服务（/ws）+ SSH/SFTP/审计/设置 + AI 对话 + MCP（--mcp-stdio 子模式）
//! - 两种构建形态（Cargo feature `pet`）：
//!   - 完整版（默认）：入口只有桌宠，主窗口 visible=false 后台预加载，双击宠物 / 宠物菜单 / 托盘按需唤起
//!   - SSH 精简版（--no-default-features）：无桌宠（不建宠物窗、不打动作包素材），
//!     启动直接进工作台 + 托盘，安装体积与常驻内存显著降低
//! - 桌宠可隐藏/唤出：宠物菜单隐藏，托盘"隐藏/显示桌宠"开关或二次启动唤回
//! - 系统托盘：打开工作台 / 桌宠开关（仅完整版）/ 退出；主窗口关闭 = 隐藏到托盘
//! - 开机自启（tauri-plugin-autostart）：启动桌面 exe 并带 --silent（不弹工作台，后台常驻）

use std::path::PathBuf;
use std::sync::Arc;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

use crate::protocol::{AgentOp, AgentResult};
use crate::stores::Stores;

pub mod ai;
pub mod db;
pub mod disk;
pub mod dl_http;
pub mod mcp;
pub mod metrics;
pub mod policy;
pub mod pool;
pub mod protocol;
pub mod server;
pub mod session;
pub mod sftp;
pub mod ssh;
pub mod stores;
pub mod toolcore;
pub mod wire;
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

// ── Tauri IPC 命令（工作台/桌宠使用）─────────────────────────────────────

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

/// 双击宠物/托盘外的回工作台入口：唤起主窗口
#[tauri::command]
fn show_main_window(app: tauri::AppHandle) {
    let data_dir = resolve_data_dir();
    append_log(&data_dir, "[dbg] show_main_window 被调用");
    match app.get_webview_window("main") {
        Some(win) => {
            append_log(&data_dir, &format!("[dbg] show 结果: {:?}", win.show()));
            let _ = win.set_focus();
        }
        None => append_log(&data_dir, "[dbg] main 窗口不存在"),
    }
}

/// 临时诊断：桌宠事件探针（修完双击问题后删除）
#[tauri::command]
fn dbg_evt(msg: String) {
    let data_dir = resolve_data_dir();
    append_log(&data_dir, &format!("[dbg] {msg}"));
}

/// 隐藏/唤出桌宠（宠物窗菜单隐藏后，经托盘开关或二次启动唤回）；
/// SSH 精简版无宠物窗（窗口查找返回 None → no-op），但命令保留注册以匹配
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

/// 文件元信息（拖拽上传面板：展示大小 / 跳过目录）
#[derive(serde::Serialize)]
struct FileStat {
    name: String,
    size: u64,
    is_dir: bool,
}

#[tauri::command]
fn stat_file(path: String) -> Result<FileStat, String> {
    let md = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    Ok(FileStat {
        name,
        size: md.len(),
        is_dir: md.is_dir(),
    })
}

/// 分块读取本地文件（拖拽上传：工作台按 256KB 拉取，经 IPC 二进制响应返回）
#[tauri::command]
fn read_file_chunk(path: String, offset: u64, len: u32) -> Result<tauri::ipc::Response, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; len as usize];
    let mut total = 0;
    while total < buf.len() {
        match f.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    buf.truncate(total);
    Ok(tauri::ipc::Response::new(buf))
}

/// Agent 自身控制（工作台设置页）：自启 / 启动时打开工作台 / 退出
fn make_agent_ctl(app: tauri::AppHandle, stores: Stores, data_dir: PathBuf) -> server::AgentCtl {
    Arc::new(move |op: AgentOp| {
        let app = app.clone();
        let stores = stores.clone();
        let data_dir = data_dir.clone();
        Box::pin(async move {
            match op.op.as_str() {
                "get-info" => AgentResult {
                    op: "get-info".into(),
                    ok: true,
                    autostart: Some(app.autolaunch().is_enabled().unwrap_or(false)),
                    open_on_start: Some(stores.settings.get_bool("app.openOnStart").unwrap_or(true)),
                    version: Some(VERSION.into()),
                    message: None,
                },
                "set-autostart" => {
                    let enabled = op.enabled.unwrap_or(false);
                    let manager = app.autolaunch();
                    let r = if enabled {
                        manager.enable()
                    } else {
                        manager.disable()
                    };
                    match r {
                        Ok(()) => AgentResult {
                            op: "set-autostart".into(),
                            ok: true,
                            autostart: Some(manager.is_enabled().unwrap_or(false)),
                            open_on_start: None,
                            version: None,
                            message: None,
                        },
                        Err(e) => AgentResult {
                            op: "set-autostart".into(),
                            ok: false,
                            autostart: Some(manager.is_enabled().unwrap_or(false)),
                            open_on_start: None,
                            version: None,
                            message: Some(e.to_string()),
                        },
                    }
                }
                "set-open-on-start" => {
                    let enabled = op.enabled.unwrap_or(true);
                    stores
                        .settings
                        .set("app.openOnStart", &serde_json::json!(enabled));
                    AgentResult {
                        op: "set-open-on-start".into(),
                        ok: true,
                        autostart: None,
                        open_on_start: Some(enabled),
                        version: None,
                        message: None,
                    }
                }
                "shutdown" => {
                    append_log(&data_dir, "[bgshell] 收到工作台退出指令，进程退出");
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
        show_main_window,
        dbg_evt,
        set_pet_visible,
        quit_app,
        stat_file,
        read_file_chunk,
        pet_pack::pet_pack_import,
        pet_pack::pet_packs_list,
        pet_pack::pet_pack_delete,
        pet_pack::pet_pack_set_enabled
    ])
}

#[cfg(not(feature = "pet"))]
fn with_ipc_commands(b: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    b.invoke_handler(tauri::generate_handler![
        show_main_window,
        dbg_evt,
        set_pet_visible,
        quit_app,
        stat_file,
        read_file_chunk
    ])
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
    let silent = std::env::args().any(|a| a == "--silent");
    let builder = tauri::Builder::default()
        // 桌面单实例：完整版唤回宠物窗（隐藏时也能找回），SSH 精简版唤起工作台主窗
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            #[cfg(feature = "pet")]
            {
                if let Some(win) = app.get_webview_window("pet") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
                sync_pet_tray_text(app);
            }
            #[cfg(not(feature = "pet"))]
            {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
        }))
        // 开机自启：启动桌面 exe 并带 --silent（后台常驻，不弹工作台）
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--silent"]),
        ))
        // 终端复制/粘贴：原生剪贴板读写（经 plugin:clipboard-manager IPC）
        .plugin(tauri_plugin_clipboard_manager::init())
        // 原生文件选择器（主机编辑：私钥文件路径浏览）
        .plugin(tauri_plugin_dialog::init());
    let app = with_ipc_commands(builder)
        .setup(move |app| {
            let data_dir = resolve_data_dir();
            append_log(&data_dir, &format!("[bgshell] v{VERSION} 启动（单进程架构）"));

            // ── 持久层 + WS 服务 ──────────────────────────────────────
            let db = db::Db::open(&data_dir.join("bitgalaxy.db"));
            db.migrate_from_json(&data_dir);
            let stores = Stores::open(db);

            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
            let agent_ctl = make_agent_ctl(app.handle().clone(), stores.clone(), data_dir.clone());
            let stores_for_server = stores.clone();
            let policy = policy::load_ai_policy(&data_dir, &stores);
            let port: u16 = std::env::var("BGSH_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(7777);
            tauri::async_runtime::spawn(async move {
                if let Err(e) = server::start_server(port, stores_for_server, policy, agent_ctl, Some(ready_tx)).await {
                    eprintln!("[bgshell] WS 服务启动失败: {e}");
                }
            });

            // ── 系统托盘 ──────────────────────────────────────────
            let show = MenuItem::with_id(app, "show", "打开工作台", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            // 桌宠开关菜单项仅完整版存在（SSH 精简版托盘只有 工作台/退出）
            #[cfg(feature = "pet")]
            let menu = {
                let pet_toggle = MenuItem::with_id(app, "pet", "隐藏桌宠", true, None::<&str>)?;
                app.manage(PetToggle(pet_toggle.clone()));
                Menu::with_items(app, &[&show, &pet_toggle, &quit])?
            };
            #[cfg(not(feature = "pet"))]
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("BitGalaxy Shell")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                    // 桌宠开关：按当前可见性取反（隐藏 ⇄ 唤出）；精简版无此菜单项
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

            // ── 工作台主窗 ─────────────────────────────────────────────
            // dev：vite dev server 可达（tauri dev / npm run dev:ui）→ 加载 devUrl；
            // 直跑 exe / release：加载随包分发的构建产物（apps/desktop/ui）。
            let main_url = if cfg!(debug_assertions) && dev_server_ready(17177) {
                WebviewUrl::External("http://localhost:17177".parse().unwrap())
            } else {
                WebviewUrl::App("index.html".into())
            };
            WebviewWindowBuilder::new(app, "main", main_url)
                .title("BitGalaxy Shell 工作台")
                // TODO(debug): 临时开 CDP 9222（首个 webview 决定共享环境）——排查完删除
                .additional_browser_args(
                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port=9222",
                )
                .inner_size(1360.0, 860.0)
                .min_inner_size(1024.0, 680.0)
                .decorations(false)
                .visible(false)
                .on_page_load(|_webview, payload| {
                    if cfg!(debug_assertions) {
                        eprintln!(
                            "[bgshell][debug] main 页面加载 {:?}: {}",
                            payload.event(),
                            payload.url()
                        );
                    }
                })
                .build()?;

            // 服务就绪后按设置决定是否展示工作台主窗（必须在主窗 build 之后启动：
            // ready 信号在 start_server 绑定端口后立即发出，早于窗口创建，先 spawn 会拿到 None）
            let app_handle = app.handle().clone();
            let open_on_start = stores.settings.get_bool("app.openOnStart").unwrap_or(true);
            tauri::async_runtime::spawn(async move {
                if ready_rx.await.is_ok() && open_on_start && !silent {
                    if let Some(win) = app_handle.get_webview_window("main") {
                        let _ = win.show();
                        let _ = win.set_focus();
                    }
                }
            });

            // ── 鲸鱼娘桌宠窗（仅完整版）：透明置顶 · 无边框 · 不进任务栏 ─────
            // 与主窗同一 dev server（workbench-ui MPA：`/` 工作台 + `/pet/` 桌宠）
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
        .on_window_event(|window, event| {
            // 关闭主窗口 → 隐藏到托盘；真正退出走托盘菜单
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("初始化 Tauri 失败");

    // 单进程架构：无需结束子进程，事件循环直接运行
    app.run(|_app, _event| {})
}
