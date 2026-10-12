//! BitGalaxy Shell 桌面应用（Tauri 2，单进程架构）：
//! - Agent 全内嵌：WS 服务（/ws）+ SSH/SFTP/审计/设置 + AI 对话 + MCP（--mcp-stdio 子模式）
//! - 入口只有桌宠：主窗口 visible=false 后台预加载，双击宠物 / 宠物菜单 / 托盘按需唤起
//! - 系统托盘：打开工作台 / 退出；主窗口关闭 = 隐藏到托盘
//! - 开机自启（tauri-plugin-autostart）：启动桌面 exe 并带 --silent（不弹工作台，桌宠/托盘常驻）

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
pub mod mcp;
pub mod metrics;
pub mod policy;
pub mod protocol;
pub mod server;
pub mod session;
pub mod sftp;
pub mod ssh;
pub mod stores;
pub mod wire;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 宠物窗尺寸（逻辑像素，pet-ui 素材 256 + 气泡余量）
const PET_W: f64 = 340.0;
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

/// 双击宠物/托盘外的回工作台入口：唤起主窗口
#[tauri::command]
fn show_main_window(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }
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

pub fn run() {
    let silent = std::env::args().any(|a| a == "--silent");
    let app = tauri::Builder::default()
        // 桌面单实例：入口只有桌宠，二次启动聚焦宠物窗后自动退出
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(win) = app.get_webview_window("pet") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        // 开机自启：启动桌面 exe 并带 --silent（桌宠/托盘常驻，不弹工作台）
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--silent"]),
        ))
        .invoke_handler(tauri::generate_handler![
            show_main_window,
            stat_file,
            read_file_chunk
        ])
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
            let port: u16 = std::env::var("BGSH_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(7777);
            tauri::async_runtime::spawn(async move {
                if let Err(e) = server::start_server(port, stores_for_server, agent_ctl, Some(ready_tx)).await {
                    eprintln!("[bgshell] WS 服务启动失败: {e}");
                }
            });

            // 服务就绪后按设置决定是否展示工作台主窗
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

            // ── 系统托盘 ──────────────────────────────────────────────
            let show = MenuItem::with_id(app, "show", "打开工作台", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
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
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // ── 鲸鱼娘桌宠窗：透明置顶 · 无边框 · 不进任务栏 ──────────
            // dev：主窗加载 vite dev server（npm run dev:ui，5177）；
            //      宠物窗加载 pet-ui dev server（npm run dev:pet，5178）。
            // release：均加载随包分发的构建产物（apps/desktop/ui）。
            let use_dev_url = cfg!(debug_assertions) && std::env::var("BGSH_PET_STATIC").is_err();
            let pet_url = if use_dev_url {
                WebviewUrl::External("http://localhost:5178/pet/".parse().unwrap())
            } else {
                WebviewUrl::App("pet/index.html".into())
            };
            let mut pet = WebviewWindowBuilder::new(app, "pet", pet_url)
                .title("鲸鱼娘桌宠")
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
