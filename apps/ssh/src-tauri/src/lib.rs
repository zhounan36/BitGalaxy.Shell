//! BitGalaxy SSH（四 EXE 架构之 SSH 客户端壳）：
//! - 独立 Tauri 桌面应用：SSH 终端 / SFTP 文件管理（含拖出下载 /dl）/ 运行状态 / 主机档案管理
//! - WS 服务（7771，BGSH_PORT 环境变量覆盖）分发全量 SSH 帧；与主程序共享 bitgalaxy.db
//!   （主机档案/已知指纹/设置，WAL + busy_timeout 兜底并发；按域拆库在 P6）
//! - AI 侧栏（连 7777 主程序）在 P5 接入

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use tauri::Manager;
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

use bg_core::policy::AiPolicy;
use bg_core::protocol::{
    self, AgentOp, AgentResult, ClientFrame, ServerFrame, SettingsResultM, DF_SFTP_UPLOAD, DF_STDIN,
};
use bg_core::stores::Stores;
use bg_core::wire::Outbound;
use bg_server::{ConnHandler, Dispatcher, WsServer};
use bg_ssh::dl_http;
use bg_ssh::pool::ConnRegistry;
use bg_ssh::session::SessionManager;
use tokio::net::TcpStream;

/// 默认端口：7771（主程序 7777 / sql 7772 / serial 7773）
const DEFAULT_PORT: u16 = 7771;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
        .open(data_dir.join("ssh-agent.log"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

// ── Tauri IPC 命令（终端/文件面板上传：本地文件元信息与分块读取）────────────

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

/// 分块读取本地文件（终端拖拽上传：前端按 256KB 拉取，经 IPC 二进制响应返回）
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

/// 拉起 AI 基座（bgshell-desktop，7777）：AI 侧栏/askAi 发现基座未运行时调用。
/// 命令内部会阻塞等待基座 WS 端口就绪（最长 15s），放阻塞线程避免卡 UI。
#[tauri::command]
async fn spawn_ai_base() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(bg_core::ai_base::ensure_ai_base)
        .await
        .map_err(|e| e.to_string())?
        .map(|_| ())
}

/// Agent 自身控制（设置弹窗）：自启 / 启动打开 / 退出
type AgentCtl = Arc<dyn Fn(AgentOp) -> BoxFuture<'static, AgentResult> + Send + Sync>;

fn make_agent_ctl(app: tauri::AppHandle, stores: Stores, data_dir: PathBuf) -> AgentCtl {
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
                    open_on_start: Some(stores.settings.get_bool("ssh.openOnStart").unwrap_or(true)),
                    version: Some(VERSION.into()),
                    message: None,
                },
                "set-autostart" => {
                    let enabled = op.enabled.unwrap_or(false);
                    let manager = app.autolaunch();
                    let r = if enabled { manager.enable() } else { manager.disable() };
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
                        .set("ssh.openOnStart", &serde_json::json!(enabled));
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
                    append_log(&data_dir, "[ssh] 收到退出指令，进程退出");
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

/// SSH 壳分发器：连接注册表 + 拖出下载令牌表（进程级单例，跨 WS 连接共享）
struct SshDispatcher {
    stores: Stores,
    registry: Arc<ConnRegistry>,
    dl: Arc<dl_http::DlTickets>,
    agent_ctl: AgentCtl,
}

impl SshDispatcher {
    fn new(port: u16, stores: Stores, policy: AiPolicy, agent_ctl: AgentCtl) -> Self {
        let registry = Arc::new(ConnRegistry::new(stores.clone(), policy.limits.idle_close_sec));
        let dl = Arc::new(dl_http::DlTickets::new(port));
        SshDispatcher { stores, registry, dl, agent_ctl }
    }
}

impl Dispatcher for SshDispatcher {
    /// /dl 请求（文件面板拖出下载）在同端口分流：消费整条连接；其余（WS 握手等）不消费字节
    fn try_take_stream<'a>(&'a self, stream: &'a mut TcpStream) -> BoxFuture<'a, bool> {
        let dl = &self.dl;
        let registry = &self.registry;
        Box::pin(async move { dl_http::try_handle(stream, dl, registry).await })
    }

    fn on_conn_open(&self, out: Outbound) -> BoxFuture<'static, Box<dyn ConnHandler>> {
        let manager = SessionManager::new(
            self.stores.clone(),
            out.clone(),
            self.registry.clone(),
            self.dl.clone(),
        );
        let conn = SshConn {
            manager,
            stores: self.stores.clone(),
            agent_ctl: self.agent_ctl.clone(),
            out,
        };
        Box::pin(async move { Box::new(conn) as Box<dyn ConnHandler> })
    }
}

/// 连接级状态：SSH 会话管理器（断开清空本连接全部会话，连接由注册表按空闲回收）
struct SshConn {
    manager: SessionManager,
    stores: Stores,
    agent_ctl: AgentCtl,
    out: Outbound,
}

impl Drop for SshConn {
    fn drop(&mut self) {
        self.manager.close_all("socket-closed");
    }
}

impl ConnHandler for SshConn {
    fn dispatch(&self, frame: ClientFrame) -> BoxFuture<'static, ()> {
        let manager = self.manager.clone();
        let stores = self.stores.clone();
        let agent_ctl = self.agent_ctl.clone();
        let out = self.out.clone();
        Box::pin(async move {
            match frame {
                ClientFrame::Open { s, m } => {
                    manager.open(&s, &m.host_id, &m);
                }
                ClientFrame::Resize { s, m } => {
                    // resize 在独立任务执行：写挂起（僵尸连接）不得阻塞 WS 帧分发循环
                    tokio::spawn(async move {
                        manager.resize(&s, m.cols, m.rows).await;
                    });
                }
                ClientFrame::Close { s, m } => {
                    let reason = m.reason.unwrap_or_else(|| "user".into());
                    manager.close(&s, &reason);
                }
                ClientFrame::ConfirmHostKey { s, m } => manager.resolve_host_key(&s, m.accepted),
                ClientFrame::RetryAuth { s, m } => manager.resolve_auth(&s, m.password),
                ClientFrame::Hosts { m } => manager.handle_hosts_op(&m),
                ClientFrame::Sftp { s, m } => manager.handle_sftp(&s, &m),
                ClientFrame::Metrics { s, m } => {
                    manager.set_metrics_interval(&s, m.interval_sec.unwrap_or(0));
                }
                ClientFrame::Probe { m } => {
                    tokio::spawn(async move {
                        manager.probe(&m.host_id).await;
                    });
                }
                ClientFrame::TestHost { m } => {
                    tokio::spawn(async move {
                        manager.test_host(m).await;
                    });
                }
                ClientFrame::Agent { m } => {
                    tokio::spawn(async move {
                        let result = agent_ctl(m).await;
                        out.send_frame(&ServerFrame::AgentResult { m: result });
                    });
                }
                ClientFrame::Settings { m } => {
                    let m = handle_settings(&stores, &m);
                    out.send_frame(&ServerFrame::SettingsResult { m });
                }
                // SSH 壳只承载 SSH 帧；AI/SQL/串口帧静默忽略（AI 侧栏 P5 接入）
                _ => {}
            }
        })
    }

    fn dispatch_data(&self, df: u8, sid: &str, payload: Vec<u8>) -> BoxFuture<'static, ()> {
        let manager = self.manager.clone();
        let sid = sid.to_string();
        Box::pin(async move {
            match df {
                DF_STDIN => {
                    // 入队即返回（会话 stdin 任务异步写入），分发循环永不被 SSH 写阻塞
                    manager.try_input(&sid, &payload);
                }
                DF_SFTP_UPLOAD => {
                    if let Some((op_id, chunk)) = protocol::decode_op_payload(&payload) {
                        manager.upload_chunk(&sid, &op_id, chunk.to_vec());
                    }
                }
                _ => {}
            }
        })
    }
}

/// settings 键值操作（m: {op, key?, value?}）。
/// 「启动时打开工作台」按壳隔离：前端沿用 app.openOnStart 键名，此处映射到
/// ssh.openOnStart（避免与主程序的启动行为互相干扰）；其余键（ai.* 等）直通共享库。
fn handle_settings(stores: &Stores, m: &serde_json::Value) -> SettingsResultM {
    let op = m.get("op").and_then(|v| v.as_str()).unwrap_or_default();
    // 壳内键名映射：app.openOnStart ⇄ ssh.openOnStart
    fn map_key(key: &str) -> &str {
        if key == "app.openOnStart" { "ssh.openOnStart" } else { key }
    }
    match op {
        "list" => {
            let mut items = stores.settings.get_all();
            if let Some(v) = items.get("ssh.openOnStart") {
                items.insert("app.openOnStart".into(), v.clone());
            }
            SettingsResultM {
                ok: true,
                items: Some(items),
                value: None,
                message: None,
            }
        }
        "get" => {
            let key = m.get("key").and_then(|v| v.as_str()).unwrap_or_default();
            SettingsResultM {
                ok: true,
                items: None,
                value: Some(stores.settings.get(map_key(key)).unwrap_or(serde_json::Value::Null)),
                message: None,
            }
        }
        "set" => {
            let key = m.get("key").and_then(|v| v.as_str()).unwrap_or_default();
            let value = m.get("value").cloned().unwrap_or(serde_json::Value::Null);
            stores.settings.set(map_key(key), &value);
            SettingsResultM { ok: true, items: None, value: None, message: None }
        }
        "delete" => {
            let key = m.get("key").and_then(|v| v.as_str()).unwrap_or_default();
            SettingsResultM {
                ok: stores.settings.delete(map_key(key)),
                items: None,
                value: None,
                message: None,
            }
        }
        _ => SettingsResultM {
            ok: false,
            items: None,
            value: None,
            message: Some("未知 settings 操作".into()),
        },
    }
}

pub fn run() {
    let silent = std::env::args().any(|a| a == "--silent");
    tauri::Builder::default()
        // 桌面单实例：二次启动唤起已有主窗（按 identifier 隔离，不影响其他三应用）
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        // 开机自启：启动自身 exe 并带 --silent（后台常驻，不弹窗口）
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--silent"]),
        ))
        // 终端复制/粘贴：原生剪贴板读写（经 plugin:clipboard-manager IPC）
        .plugin(tauri_plugin_clipboard_manager::init())
        // 原生文件选择器（主机编辑：私钥文件路径浏览）
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![stat_file, read_file_chunk, spawn_ai_base])
        .setup(move |app| {
            let data_dir = resolve_data_dir();
            let _ = std::fs::create_dir_all(&data_dir);
            append_log(&data_dir, &format!("[ssh] 启动 v{VERSION}"));

            let port = std::env::var("BGSH_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(DEFAULT_PORT);

            // 共享持久层（P6.5 按域拆库：hosts/settings + 旧库迁移）：多进程并发由 WAL + busy_timeout 兜底
            let stores = Stores::open_domains(&data_dir);
            let policy = bg_core::policy::load_ai_policy(&data_dir, &stores);

            let agent_ctl = make_agent_ctl(app.handle().clone(), stores.clone(), data_dir);
            let dispatcher = Arc::new(SshDispatcher::new(port, stores, policy, agent_ctl));
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = WsServer::start(port, dispatcher, Some(ready_tx)).await {
                    eprintln!("[ssh] WS 服务启动失败: {e}");
                }
            });

            // WS 就绪后展示主窗（除非 --silent：自启后台常驻，二次启动唤起）。
            // conf 中 visible=false：先服务后窗口，避免前端连不上 7771
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if ready_rx.await.is_ok() && !silent {
                    if let Some(win) = app_handle.get_webview_window("main") {
                        let _ = win.show();
                        let _ = win.set_focus();
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("bgshell-ssh 启动失败");
}
