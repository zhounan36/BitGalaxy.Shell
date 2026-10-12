//! BitGalaxy Shell 桌面壳（Tauri 2）：
//! - 启动时以 sidecar 方式拉起 bgshell-agent --silent（壳即窗口，agent 不再开浏览器）
//! - 系统托盘：打开工作台 / 退出（退出时结束 agent 子进程）
//! - 主窗口关闭 = 隐藏到托盘（agent 后台继续运行）
//! - 鲸鱼娘桌宠窗（pet）：透明置顶小窗，加载 pet-ui；双击宠物回到工作台主窗

use std::sync::Mutex;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// agent 子进程句柄（退出时 kill）
struct AgentChild(Mutex<Option<CommandChild>>);

/// 宠物窗尺寸（逻辑像素，pet-ui 素材 256 + 气泡余量）
const PET_W: f64 = 340.0;
const PET_H: f64 = 420.0;

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

pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(AgentChild(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            show_main_window,
            stat_file,
            read_file_chunk
        ])
        .setup(|app| {
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

            // ── sidecar：拉起 bgshell-agent --silent ─────────────────
            let sidecar = app
                .shell()
                .sidecar("bgshell-agent")
                .expect("sidecar 未配置（先执行 npm run desktop:prep）");
            let (mut rx, child) = sidecar
                .args(["--silent"])
                .spawn()
                .expect("bgshell-agent 启动失败");
            *app.state::<AgentChild>().0.lock().unwrap() = Some(child);

            // 转发 agent stderr 到调试输出（GUI 子系统无控制台）
            tauri::async_runtime::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if let CommandEvent::Stderr(line) = event {
                        eprintln!("[agent] {}", String::from_utf8_lossy(&line));
                    }
                }
            });

            // ── 鲸鱼娘桌宠窗：透明置顶 · 无边框 · 不进任务栏 ──────────
            // dev：加载 vite dev server（另开 npm run dev:pet）；
            // release / BGSH_PET_STATIC=1：加载随包分发的 pet-ui 构建产物。
            let use_dev_url =
                cfg!(debug_assertions) && std::env::var("BGSH_PET_STATIC").is_err();
            let pet_url = if use_dev_url {
                WebviewUrl::External("http://localhost:5178/".parse().unwrap())
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
            // 关闭主窗口 → 隐藏到托盘；真正退出走托盘菜单（会结束 agent）
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("初始化 Tauri 失败");

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            // 真正退出时结束 agent 子进程
            if let Some(child) = app.state::<AgentChild>().0.lock().unwrap().take() {
                let _ = child.kill();
            }
        }
    })
}
