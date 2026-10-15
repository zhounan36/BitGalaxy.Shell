//! BitGalaxy SQL（四 EXE 架构之 SQL 客户端壳）：
//! - 独立 Tauri 桌面应用：直连 MySQL/PostgreSQL/本地 SQLite（驱动宿主随包内置）
//! - WS 服务（7772，BGSH_PORT 环境变量覆盖）只分发 SQL 帧与连接档案 CRUD
//! - AI 侧栏（连 7777 主程序）在 P5 接入

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use tauri::Manager;

use bg_core::protocol::ClientFrame;
use bg_core::wire::Outbound;
use bg_server::{ConnHandler, Dispatcher, WsServer};
use bg_sql::SqlHandler;

/// 默认端口：7772（主程序 7777 / ssh 7771 / serial 7773）
const DEFAULT_PORT: u16 = 7772;

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

pub fn append_log(data_dir: &PathBuf, msg: &str) {
    let line = format!("{} {msg}\n", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("sql-agent.log"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

/// SQL 壳分发器：SqlHandler 进程级单例（驱动宿主 + 连接档案跨连接共享）
struct SqlDispatcher {
    sql: Arc<SqlHandler>,
}

impl Dispatcher for SqlDispatcher {
    fn on_conn_open(&self, out: Outbound) -> BoxFuture<'static, Box<dyn ConnHandler>> {
        let sql_ws = self.sql.new_conn();
        let conn = SqlConn {
            sql: self.sql.clone(),
            sql_ws,
            out,
        };
        Box::pin(async move { Box::new(conn) as Box<dyn ConnHandler> })
    }
}

/// 连接级状态：SQL 代际（断开只清本连接的数据库会话/驱动进程）
struct SqlConn {
    sql: Arc<SqlHandler>,
    sql_ws: u64,
    out: Outbound,
}

impl Drop for SqlConn {
    fn drop(&mut self) {
        self.sql.close_all(self.sql_ws);
    }
}

impl ConnHandler for SqlConn {
    fn dispatch(&self, frame: ClientFrame) -> BoxFuture<'static, ()> {
        let sql = self.sql.clone();
        let sql_ws = self.sql_ws;
        let out = self.out.clone();
        Box::pin(async move {
            match frame {
                ClientFrame::Sql { s, m } => sql.handle_op(sql_ws, &s, &m, &out),
                ClientFrame::SqlConns { m } => sql.handle_conns(&m, &out),
                // SQL 壳只承载 SQL 帧；其余帧（hosts/sftp/serial/…）静默忽略
                _ => {}
            }
        })
    }

    fn dispatch_data(&self, _df: u8, _sid: &str, _payload: Vec<u8>) -> BoxFuture<'static, ()> {
        // SQL 帧全部为文本控制帧，无二进制数据帧
        Box::pin(async {})
    }
}

pub fn run() {
    // single-instance：二次启动唤起已有主窗（按 identifier 隔离，不影响其他三应用）
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|_app| {
            let data_dir = resolve_data_dir();
            let _ = std::fs::create_dir_all(&data_dir);
            append_log(&data_dir, &format!("[sql] 启动 v{}", env!("CARGO_PKG_VERSION")));

            let port = std::env::var("BGSH_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(DEFAULT_PORT);

            let sql = Arc::new(SqlHandler::new(&data_dir));
            let dispatcher = Arc::new(SqlDispatcher { sql });
            tauri::async_runtime::spawn(async move {
                if let Err(e) = WsServer::start(port, dispatcher, None).await {
                    eprintln!("[sql] WS 服务启动失败: {e}");
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("bgshell-sql 启动失败");
}
