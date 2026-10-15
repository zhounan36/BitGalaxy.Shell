//! desktop 壳的全量路由（P1 过渡：行为与单进程版完全一致）。
//! bg-server 泛化后，本文件只负责组装 desktop 的路由表：
//! SSH/SFTP/串口/SQL/AI/设置/Agent 控制全部在本进程内分发。
use std::sync::Arc;

use futures_util::future::BoxFuture;

use bg_ai::ai::AiHandler;
use bg_core::policy::AiPolicy;
use bg_core::protocol::{
    self, AgentResult, ClientFrame, ServerFrame, SettingsResultM, DF_SFTP_UPLOAD, DF_STDIN,
};
use bg_core::stores::Stores;
use bg_core::wire::{OutHub, Outbound};
use bg_serial::serial;
use bg_serial::SerialSessionManager;
use bg_server::{ConnHandler, Dispatcher};
use bg_ssh::dl_http;
use bg_ssh::pool::ConnRegistry;
use bg_ssh::session::SessionManager;
use bg_sql::SqlHandler;
use tokio::net::TcpStream;

/// Agent 自身控制（桌面形态注入：自启/退出等）
pub type AgentCtl = Arc<dyn Fn(protocol::AgentOp) -> BoxFuture<'static, AgentResult> + Send + Sync>;

/// desktop 全量分发器：进程级单例集合（注册表/AI/SQL/拖出下载令牌表）
pub struct DesktopDispatcher {
    stores: Stores,
    agent_ctl: AgentCtl,
    registry: Arc<ConnRegistry>,
    dl: Arc<dl_http::DlTickets>,
    ai: Arc<AiHandler>,
    sql: Arc<SqlHandler>,
}

impl DesktopDispatcher {
    pub fn new(
        port: u16,
        stores: Stores,
        policy: AiPolicy,
        agent_ctl: AgentCtl,
        data_dir: std::path::PathBuf,
    ) -> Self {
        // 全局连接注册表：本进程所有 WS 连接共享（工作台会话与 AI 工具同主机复用连接；
        // MCP stdio 为独立子进程，自建实例）
        let registry = Arc::new(ConnRegistry::new(stores.clone(), policy.limits.idle_close_sec));
        // 拖出下载能力令牌表（dl-open 签发，HTTP /dl 端点消费）：进程级单例
        let dl = Arc::new(dl_http::DlTickets::new(port));
        // AiHandler 进程级单例：审批卡片 / 对话状态跨 WS 连接（断线重连后的新连接）
        // 依然路由到同一份登记表——否则重连后批准回包会落进空表被静默丢弃，
        // 120s 审批超时被误判为「用户拒绝」。AI 帧经 OutHub 广播到所有活跃连接。
        let out_hub = OutHub::default();
        let ai = Arc::new(AiHandler::new(stores.clone(), out_hub, policy, registry.clone()));
        // SQL 模块进程级单例（驱动宿主进程跨 WS 连接共享）
        let sql = Arc::new(SqlHandler::new(&data_dir));
        DesktopDispatcher { stores, agent_ctl, registry, dl, ai, sql }
    }
}

impl Dispatcher for DesktopDispatcher {
    /// /dl 请求（拖出下载）在同端口分流：消费整条连接；其余（WS 握手等）不消费字节
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
        // 串口会话管理器（P3 剥离）：与 SSH 会话平行的独立表
        let serial_mgr = SerialSessionManager::new(self.stores.clone(), out.clone());
        // 本连接加入 AI 广播组（断开时由 OutHub 发送路径自动清理）
        self.ai.register_conn(out.clone());
        // SQL 会话的连接代际：单例共享，断开只清本连接的数据库会话
        let sql_ws = self.sql.new_conn();
        let conn = DesktopConn {
            manager,
            serial: serial_mgr,
            ai: self.ai.clone(),
            sql: self.sql.clone(),
            sql_ws,
            stores: self.stores.clone(),
            agent_ctl: self.agent_ctl.clone(),
            out,
        };
        Box::pin(async move { Box::new(conn) as Box<dyn ConnHandler> })
    }
}

/// 连接级状态：会话管理器 + SQL 代际 + 各单例引用
struct DesktopConn {
    manager: SessionManager,
    serial: SerialSessionManager,
    ai: Arc<AiHandler>,
    sql: Arc<SqlHandler>,
    sql_ws: u64,
    stores: Stores,
    agent_ctl: AgentCtl,
    out: Outbound,
}

impl Drop for DesktopConn {
    fn drop(&mut self) {
        self.manager.close_all("socket-closed");
        self.serial.close_all("socket-closed");
        self.sql.close_all(self.sql_ws);
        // 注意：不撤 AI 审批——AiHandler 是进程级的，断线重连后的新连接
        // 依然可以回包；审批的 fail-closed 由 120s 超时兜底
    }
}

impl ConnHandler for DesktopConn {
    fn dispatch(&self, frame: ClientFrame) -> BoxFuture<'static, ()> {
        let manager = self.manager.clone();
        let serial_mgr = self.serial.clone();
        let ai = self.ai.clone();
        let sql = self.sql.clone();
        let sql_ws = self.sql_ws;
        let stores = self.stores.clone();
        let agent_ctl = self.agent_ctl.clone();
        let out = self.out.clone();
        Box::pin(async move {
            match frame {
                ClientFrame::Open { s, m } => {
                    // 串口会话（kind=serial）走独立管理器（本机 COM 口，不经 SSH 注册表）
                    if m.kind.as_deref() == Some("serial") {
                        tokio::spawn(async move {
                            serial_mgr.open(&s, m.serial).await;
                        });
                    } else {
                        manager.open(&s, &m.host_id, &m);
                    }
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
                    serial_mgr.close(&s, &reason);
                    // SQL tab 关闭：一并清理该会话的数据库连接/驱动进程/临时文件
                    sql.close_session(&s);
                }
                ClientFrame::ConfirmHostKey { s, m } => manager.resolve_host_key(&s, m.accepted),
                ClientFrame::RetryAuth { s, m } => manager.resolve_auth(&s, m.password),
                ClientFrame::Hosts { m } => manager.handle_hosts_op(&m),
                ClientFrame::Sftp { s, m } => manager.handle_sftp(&s, &m),
                ClientFrame::Sql { s, m } => sql.handle_op(sql_ws, &s, &m, &out),
                ClientFrame::SqlConns { m } => sql.handle_conns(&m, &out),
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
                ClientFrame::Ai { m } => ai.handle_ai(m),
                ClientFrame::AiApprove { m } => ai.handle_approve(&m),
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
                    // 控制调用走独立任务：不阻塞 WS 帧分发循环（写/信号查询可能挂起）
                    tokio::spawn(async move {
                        serial_mgr.serial_ctl(&s, m).await;
                    });
                }
                ClientFrame::Settings { m } => {
                    let m = handle_settings(&stores, &m);
                    out.send_frame(&ServerFrame::SettingsResult { m });
                }
            }
        })
    }

    fn dispatch_data(&self, df: u8, sid: &str, payload: Vec<u8>) -> BoxFuture<'static, ()> {
        let manager = self.manager.clone();
        let serial_mgr = self.serial.clone();
        let sid = sid.to_string();
        Box::pin(async move {
            match df {
                DF_STDIN => {
                    // 入队即返回（会话 stdin 任务异步写入），分发循环永不被 SSH 写阻塞。
                    // SSH 会话未命中时回落串口会话（同一终端面板，按 sid 查两张表）
                    if manager.try_input(&sid, &payload) {
                        return;
                    }
                    serial_mgr.input(&sid, &payload);
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

/// settings 键值操作（m: {op, key?, value?}）
fn handle_settings(stores: &Stores, m: &serde_json::Value) -> SettingsResultM {
    let op = m.get("op").and_then(|v| v.as_str()).unwrap_or_default();
    match op {
        "list" => SettingsResultM {
            ok: true,
            items: Some(stores.settings.get_all()),
            value: None,
            message: None,
        },
        "get" => {
            let key = m.get("key").and_then(|v| v.as_str()).unwrap_or_default();
            SettingsResultM {
                ok: true,
                items: None,
                value: Some(stores.settings.get(key).unwrap_or(serde_json::Value::Null)),
                message: None,
            }
        }
        "set" => {
            let key = m.get("key").and_then(|v| v.as_str()).unwrap_or_default();
            let value = m.get("value").cloned().unwrap_or(serde_json::Value::Null);
            stores.settings.set(key, &value);
            SettingsResultM { ok: true, items: None, value: None, message: None }
        }
        "delete" => {
            let key = m.get("key").and_then(|v| v.as_str()).unwrap_or_default();
            SettingsResultM {
                ok: stores.settings.delete(key),
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
