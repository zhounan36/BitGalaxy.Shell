//! Agent 接入面①：单端口 WS（/ws）承载内嵌工作台与桌宠的帧通道。
//! Origin 白名单仅放行本机 UI（Tauri webview / vite dev / agent 同源页面）。
use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

use crate::ai::AiHandler;
use crate::protocol::{
    self, AgentResult, ClientFrame, ServerFrame, SettingsResultM, DF_SFTP_UPLOAD, DF_STDIN,
};
use crate::session::SessionManager;
use crate::stores::Stores;
use crate::wire::{OutMsg, Outbound};

/// Agent 自身控制（桌面形态注入：自启/退出等）
pub type AgentCtl = Arc<dyn Fn(protocol::AgentOp) -> BoxFuture<'static, AgentResult> + Send + Sync>;

fn allowed_origins(port: u16) -> Vec<String> {
    vec![
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
        // vite dev（workbench-ui 5177 / pet-ui 5178 代理 /ws）
        "http://localhost:5177".into(),
        "http://127.0.0.1:5177".into(),
        "http://localhost:5178".into(),
        "http://127.0.0.1:5178".into(),
        // Tauri 桌面壳 webview
        "http://tauri.localhost".into(),
        "tauri://localhost".into(),
    ]
}

/// 启动 WS 服务；绑定成功后通过 ready 通知（端口被占用返回 Err）
pub async fn start_server(
    port: u16,
    stores: Stores,
    agent_ctl: AgentCtl,
    ready: Option<oneshot::Sender<()>>,
) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("端口 {port} 绑定失败: {e}"))?;
    if let Some(tx) = ready {
        let _ = tx.send(());
    }
    let allowed = allowed_origins(port);
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let stores = stores.clone();
        let agent_ctl = agent_ctl.clone();
        let allowed = allowed.clone();
        tokio::spawn(async move {
            if cfg!(debug_assertions) {
                eprintln!("[bgshell][debug] WS 接入新连接");
            }
            handle_conn(stream, stores, agent_ctl, allowed).await;
        });
    }
}

async fn handle_conn(
    stream: tokio::net::TcpStream,
    stores: Stores,
    agent_ctl: AgentCtl,
    allowed: Vec<String>,
) {
    // Origin 校验后完成 WS 握手
    let ws = {
        let allowed_ref = &allowed;
        let cb = move |req: &Request, resp: Response| {
            let origin = req
                .headers()
                .get("origin")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("null");
            if allowed_ref.iter().any(|a| a == origin) {
                Ok(resp)
            } else {
                // 注意：Response 别名 body 为 ()；错误响应需要显式 ErrorResponse（body 为 Option<String>）
                let denied: tokio_tungstenite::tungstenite::http::Response<Option<String>> =
                    tokio_tungstenite::tungstenite::http::Response::builder()
                        .status(403)
                        .body(Some("origin denied".to_string()))
                        .unwrap_or_else(|_| {
                            tokio_tungstenite::tungstenite::http::Response::new(Some(String::new()))
                        });
                Err(denied)
            }
        };
        match tokio_tungstenite::accept_hdr_async(stream, cb).await {
            Ok(ws) => ws,
            Err(_) => return,
        }
    };

    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<OutMsg>();
    let outbound = Outbound { tx: tx.clone() };

    // 写任务：控制帧（文本）与数据帧（二进制）统一出站
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let out = match msg {
                OutMsg::Text(t) => Message::Text(t.into()),
                OutMsg::Binary(b) => Message::Binary(b.into()),
            };
            if sink.send(out).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let manager = SessionManager::new(stores.clone(), outbound.clone());
    let ai = AiHandler::new(stores.clone(), outbound.clone());

    while let Some(msg) = source.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };
        match msg {
            Message::Text(text) => {
                let Ok(frame) = serde_json::from_str::<ClientFrame>(text.as_str()) else {
                    continue;
                };
                dispatch(&manager, &ai, &stores, &agent_ctl, &outbound, frame).await;
            }
            Message::Binary(data) => {
                let Some((frame_type, sid, payload)) = protocol::decode_data_frame(&data) else {
                    continue;
                };
                match frame_type {
                    DF_STDIN => {
                        let data = payload.to_vec();
                        manager.input(&sid, &data).await;
                    }
                    DF_SFTP_UPLOAD => {
                        if let Some((op_id, chunk)) = protocol::decode_op_payload(payload) {
                            manager.upload_chunk(&sid, &op_id, chunk.to_vec());
                        }
                    }
                    _ => {}
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    manager.close_all("socket-closed");
    ai.dispose();
}

async fn dispatch(
    manager: &SessionManager,
    ai: &AiHandler,
    stores: &Stores,
    agent_ctl: &AgentCtl,
    out: &Outbound,
    frame: ClientFrame,
) {
    match frame {
        ClientFrame::Open { s, m } => manager.open(&s, &m.host_id, &m),
        ClientFrame::Resize { s, m } => {
            manager.resize(&s, m.cols, m.rows).await;
        }
        ClientFrame::Close { s, m } => {
            let reason = m.reason.unwrap_or_else(|| "user".into());
            manager.close(&s, &reason);
        }
        ClientFrame::ConfirmHostKey { s, m } => manager.resolve_host_key(&s, m.accepted),
        ClientFrame::Hosts { m } => manager.handle_hosts_op(&m),
        ClientFrame::Sftp { s, m } => manager.handle_sftp(&s, &m),
        ClientFrame::Metrics { s, m } => {
            manager.set_metrics_interval(&s, m.interval_sec.unwrap_or(0));
        }
        ClientFrame::Probe { m } => {
            let manager = manager.clone();
            tokio::spawn(async move {
                manager.probe(&m.host_id).await;
            });
        }
        ClientFrame::Agent { m } => {
            let ctl = agent_ctl.clone();
            let out = out.clone();
            tokio::spawn(async move {
                let result = ctl(m).await;
                out.send_frame(&ServerFrame::AgentResult { m: result });
            });
        }
        ClientFrame::Ai { m } => ai.handle_ai(m),
        ClientFrame::AiApprove { m } => ai.handle_approve(&m),
        ClientFrame::Settings { m } => {
            let m = handle_settings(stores, &m);
            out.send_frame(&ServerFrame::SettingsResult { m });
        }
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
