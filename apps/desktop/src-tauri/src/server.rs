//! AI 基座路由（P6 后台服务化）：只承载 AI / 审批 / 设置 / Agent 控制 / 主机档案帧。
//! SSH/SFTP/串口/SQL 数据面帧全部静默忽略（各归三工具壳，7771/7772/7773）。
//! - AiHandler 进程级单例 + OutHub 广播：宠物窗与三工具侧栏同收流式，任一处可审批
//! - ConnRegistry 保留：AI 远程工具（exec_command/SFTP）与 MCP 复用 SSH 连接池
//! - 主机档案 CRUD 直连 HostStore（桌宠菜单读取主机列表；编辑在三工具壳）
use std::sync::Arc;

use futures_util::future::BoxFuture;

use bg_ai::ai::AiHandler;
use bg_core::policy::AiPolicy;
use bg_core::protocol::{
    AgentOp, AgentResult, ClientFrame, HostInput, HostsResultM, ServerFrame, SettingsResultM,
};
use bg_core::stores::Stores;
use bg_core::wire::{OutHub, Outbound};
use bg_server::{ConnHandler, Dispatcher};
use bg_ssh::pool::ConnRegistry;

/// Agent 自身控制（桌面形态注入：退出基座等）
pub type AgentCtl = Arc<dyn Fn(AgentOp) -> BoxFuture<'static, AgentResult> + Send + Sync>;

/// AI 基座分发器：进程级单例集合（SSH 连接注册表 + AI 处理器）
pub struct DesktopDispatcher {
    stores: Stores,
    agent_ctl: AgentCtl,
    registry: Arc<ConnRegistry>,
    ai: Arc<AiHandler>,
}

impl DesktopDispatcher {
    pub fn new(stores: Stores, policy: AiPolicy, agent_ctl: AgentCtl) -> Self {
        // SSH 连接注册表：AI 远程工具（exec_command/SFTP）经此复用连接；
        // MCP stdio 为独立子进程，自建实例
        let registry = Arc::new(ConnRegistry::new(stores.clone(), policy.limits.idle_close_sec));
        // AiHandler 进程级单例：审批卡片 / 对话状态跨 WS 连接（断线重连后的新连接）
        // 依然路由到同一份登记表——否则重连后批准回包会落进空表被静默丢弃，
        // 120s 审批超时被误判为「用户拒绝」。AI 帧经 OutHub 广播到所有活跃连接。
        let out_hub = OutHub::default();
        let ai = Arc::new(AiHandler::new(stores.clone(), out_hub, policy, registry.clone()));
        DesktopDispatcher { stores, agent_ctl, registry, ai }
    }
}

impl Dispatcher for DesktopDispatcher {
    fn on_conn_open(&self, out: Outbound) -> BoxFuture<'static, Box<dyn ConnHandler>> {
        // 本连接加入 AI 广播组（断开时由 OutHub 发送路径自动清理）
        self.ai.register_conn(out.clone());
        let conn = DesktopConn {
            stores: self.stores.clone(),
            registry: self.registry.clone(),
            ai: self.ai.clone(),
            agent_ctl: self.agent_ctl.clone(),
            out,
        };
        Box::pin(async move { Box::new(conn) as Box<dyn ConnHandler> })
    }
}

/// 连接级状态（基座无会话数据面，连接断开无需清理会话；
/// 不撤 AI 审批——AiHandler 是进程级的，断线重连后的新连接依然可以回包）
struct DesktopConn {
    stores: Stores,
    registry: Arc<ConnRegistry>,
    ai: Arc<AiHandler>,
    agent_ctl: AgentCtl,
    out: Outbound,
}

impl ConnHandler for DesktopConn {
    fn dispatch(&self, frame: ClientFrame) -> BoxFuture<'static, ()> {
        let stores = self.stores.clone();
        let registry = self.registry.clone();
        let ai = self.ai.clone();
        let agent_ctl = self.agent_ctl.clone();
        let out = self.out.clone();
        Box::pin(async move {
            match frame {
                ClientFrame::Ai { m } => ai.handle_ai(m),
                ClientFrame::AiApprove { m } => ai.handle_approve(&m),
                ClientFrame::Hosts { m } => handle_hosts_op(&stores, &registry, &m, &out),
                ClientFrame::Settings { m } => {
                    let m = handle_settings(&stores, &m);
                    out.send_frame(&ServerFrame::SettingsResult { m });
                }
                ClientFrame::Agent { m } => {
                    tokio::spawn(async move {
                        let result = agent_ctl(m).await;
                        out.send_frame(&ServerFrame::AgentResult { m: result });
                    });
                }
                // 基座只承载 AI/审批/设置/主机/Agent 帧；
                // SSH/SFTP/串口/SQL 数据面帧静默忽略（归三工具壳）
                _ => {}
            }
        })
    }

    fn dispatch_data(&self, _df: u8, _sid: &str, _payload: Vec<u8>) -> BoxFuture<'static, ()> {
        // 基座无二进制数据面（stdin/SFTP 上传归各工具壳）
        Box::pin(async {})
    }
}

/// 主机档案 CRUD（直连 HostStore；变更时驱逐连接池旧连接，避免改密后残留旧凭据）。
/// 与 bg-ssh session.rs 的 handle_hosts_op 行为一致（桌宠菜单 / 工具侧栏共用）。
fn handle_hosts_op(
    stores: &Stores,
    registry: &ConnRegistry,
    m: &serde_json::Value,
    out: &Outbound,
) {
    let op = m.get("op").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let reply = |m: HostsResultM| out.send_frame(&ServerFrame::Hosts { m });
    match op.as_str() {
        "list" => reply(HostsResultM {
            op,
            ok: true,
            hosts: Some(stores.hosts.list()),
            host: None,
            message: None,
        }),
        "create" | "update" => {
            let input = serde_json::from_value::<HostInput>(m.get("host").cloned().unwrap_or(serde_json::Value::Null));
            match input {
                Ok(input) => {
                    let result = if op == "create" {
                        stores.hosts.create(&input).map(Some)
                    } else {
                        let id = m.get("host").and_then(|h| h.get("id")).and_then(|v| v.as_str()).unwrap_or_default();
                        stores.hosts.update(id, &input)
                    };
                    match result {
                        Ok(Some(host)) => {
                            // 主机档案变更（改密/改址/改端口）：驱逐池内旧连接
                            if op == "update" {
                                registry.evict(&host.id, "host-updated");
                            }
                            reply(HostsResultM { op, ok: true, hosts: None, host: Some(host), message: None })
                        }
                        Ok(None) => reply(HostsResultM {
                            op,
                            ok: false,
                            hosts: None,
                            host: None,
                            message: Some("主机档案不存在".into()),
                        }),
                        Err(e) => reply(HostsResultM { op, ok: false, hosts: None, host: None, message: Some(e.to_string()) }),
                    }
                }
                Err(e) => reply(HostsResultM { op, ok: false, hosts: None, host: None, message: Some(e.to_string()) }),
            }
        }
        "delete" => {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            let ok = stores.hosts.delete(id);
            if ok {
                registry.evict(id, "host-deleted");
            }
            reply(HostsResultM {
                op,
                ok,
                hosts: None,
                host: None,
                message: (!ok).then(|| "主机档案不存在".to_string()),
            });
        }
        _ => {}
    }
}

/// settings 键值操作（m: {op, key?, value?}）：AI 配置（ai.*）等全库直通
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
