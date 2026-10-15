//! BitGalaxy Serial（四 EXE 架构之串口工具壳）：
//! - 独立 Tauri 桌面应用：本机 COM 口枚举 + 双工调试（SerialDebugPane 为主体）
//! - WS 服务（7773，BGSH_PORT 环境变量覆盖）只分发串口帧（serial-list/open/ctl/stdin/close）
//! - AI 侧栏（连 7777 主程序）在 P5 接入

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use tauri::Manager;

use bg_core::protocol::{self, ClientFrame, ServerFrame};
use bg_core::stores::Stores;
use bg_core::wire::Outbound;
use bg_serial::{serial, SerialSessionManager};
use bg_server::{ConnHandler, Dispatcher, WsServer};

/// 默认端口：7773（主程序 7777 / ssh 7771 / sql 7772）
const DEFAULT_PORT: u16 = 7773;

/// 数据目录：与主程序共享 %APPDATA%\bitgalaxy-shell
pub fn resolve_data_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("bitgalaxy-shell");
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".local").join("state").join("bitgalaxy-shell")
}

/// 拉起 AI 基座（bgshell-desktop，7777）：AI 侧栏/askAi 发现基座未运行时调用。
/// 命令内部会阻塞等待基座 WS 端口就绪（最长 15s），放阻塞线程避免卡 UI。
#[tauri::command]
async fn spawn_ai_base() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(bg_core::ai_base::ensure_ai_base)
        .await
        .map_err(|e| e.to_string())?
        .map(|_| ())
}

/// 写文本文件（串口调试记录落盘：前端收集帧日志，保存对话框定路径后经此写入）
#[tauri::command]
fn write_text_file(path: String, contents: String) -> Result<(), String> {
    std::fs::write(&path, contents).map_err(|e| e.to_string())
}

pub fn append_log(data_dir: &PathBuf, msg: &str) {
    let line = format!("{} {msg}\n", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("serial-agent.log"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

/// 串口壳分发器：Stores（审计）进程级共享
struct SerialDispatcher {
    stores: Stores,
}

impl Dispatcher for SerialDispatcher {
    fn on_conn_open(&self, out: Outbound) -> BoxFuture<'static, Box<dyn ConnHandler>> {
        let serial_mgr = SerialSessionManager::new(self.stores.clone(), out.clone());
        let conn = SerialConn {
            serial: serial_mgr,
            out,
        };
        Box::pin(async move { Box::new(conn) as Box<dyn ConnHandler> })
    }
}

/// 连接级状态：串口会话管理器（断开清空本连接全部串口会话）
struct SerialConn {
    serial: SerialSessionManager,
    out: Outbound,
}

impl Drop for SerialConn {
    fn drop(&mut self) {
        self.serial.close_all("socket-closed");
    }
}

impl ConnHandler for SerialConn {
    fn dispatch(&self, frame: ClientFrame) -> BoxFuture<'static, ()> {
        let serial_mgr = self.serial.clone();
        let out = self.out.clone();
        Box::pin(async move {
            match frame {
                ClientFrame::Open { s, m } => {
                    if m.kind.as_deref() == Some("serial") {
                        tokio::spawn(async move {
                            serial_mgr.open(&s, m.serial).await;
                        });
                    }
                }
                ClientFrame::Close { s, m } => {
                    let reason = m.reason.unwrap_or_else(|| "user".into());
                    serial_mgr.close(&s, &reason);
                }
                ClientFrame::SerialList => {
                    let m = match serial::list_ports() {
                        Ok(ports) => protocol::SerialPortsM {
                            ok: true,
                            ports: Some(ports),
                            message: None,
                        },
                        Err(e) => protocol::SerialPortsM {
                            ok: false,
                            ports: None,
                            message: Some(e),
                        },
                    };
                    out.send_frame(&ServerFrame::SerialPorts { m });
                }
                ClientFrame::SerialCtl { s, m } => {
                    tokio::spawn(async move {
                        serial_mgr.serial_ctl(&s, m).await;
                    });
                }
                // 串口壳只承载串口帧；其余帧（hosts/sftp/sql/ai/…）静默忽略
                _ => {}
            }
        })
    }

    fn dispatch_data(&self, df: u8, sid: &str, payload: Vec<u8>) -> BoxFuture<'static, ()> {
        let serial_mgr = self.serial.clone();
        let sid = sid.to_string();
        Box::pin(async move {
            if df == protocol::DF_STDIN {
                serial_mgr.input(&sid, &payload);
            }
        })
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![spawn_ai_base, write_text_file])
        .setup(|_app| {
            let data_dir = resolve_data_dir();
            let _ = std::fs::create_dir_all(&data_dir);
            append_log(&data_dir, &format!("[serial] 启动 v{}", env!("CARGO_PKG_VERSION")));

            let port = std::env::var("BGSH_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(DEFAULT_PORT);

            let db = bg_core::db::Db::open(&data_dir.join("bitgalaxy.db"));
            let stores = Stores::open(db);
            let dispatcher = Arc::new(SerialDispatcher { stores });
            tauri::async_runtime::spawn(async move {
                if let Err(e) = WsServer::start(port, dispatcher, None).await {
                    eprintln!("[serial] WS 服务启动失败: {e}");
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("bgshell-serial 启动失败");
}
