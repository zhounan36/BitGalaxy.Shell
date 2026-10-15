//! 单端口 WS（/ws）帧通道：握手 / Origin 白名单 / 写任务隔离 / 数据帧解包。
//! 路由全部交给 Dispatcher（进程级）+ ConnHandler（连接级）——由各壳组装。

use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

use bg_core::protocol::ClientFrame;
use bg_core::wire::{OutMsg, Outbound};

/// 进程级路由器：持有各壳的单例（会话管理器 / AI / SQL / 设置存储等）。
pub trait Dispatcher: Send + Sync + 'static {
    /// 新 TCP 连接的原始流分流钩子（如 /dl HTTP 下载端点）。
    /// 返回 true 表示该连接已被整体消费（不再进行 WS 握手）。
    /// 默认不分流。
    fn try_take_stream<'a>(
        &'a self,
        _stream: &'a mut TcpStream,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// WS 连接建立（握手完成、Outbound 就绪）：注册广播、建连接级状态。
    /// 返回该连接的帧处理器；断开时 drop。
    fn on_conn_open(&self, out: Outbound) -> BoxFuture<'static, Box<dyn ConnHandler>>;
}

/// 连接级帧处理器：持有本连接的状态（会话管理器 / SQL 连接代际等）。
pub trait ConnHandler: Send + 'static {
    /// 文本帧（已解析为 ClientFrame）
    fn dispatch(&self, frame: ClientFrame) -> BoxFuture<'static, ()>;
    /// 二进制数据帧（df + sessionId + payload 已解出；未识别的 df 不会到达这里）
    fn dispatch_data(&self, df: u8, sid: &str, payload: Vec<u8>) -> BoxFuture<'static, ()>;
}

fn allowed_origins(port: u16) -> Vec<String> {
    vec![
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
        // vite dev（workbench-ui 17177 MPA：各入口经代理或直连 /ws）
        "http://localhost:17177".into(),
        "http://127.0.0.1:17177".into(),
        // Tauri 桌面壳 webview
        "http://tauri.localhost".into(),
        "tauri://localhost".into(),
    ]
}

/// WS 服务：accept → 分流钩子 → 握手 → 连接建立 → 帧循环。
/// 绑定成功后通过 ready 通知（端口被占用返回 Err）。
pub struct WsServer;

impl WsServer {
    pub async fn start(
        port: u16,
        dispatcher: Arc<dyn Dispatcher>,
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
            let dispatcher = dispatcher.clone();
            let allowed = allowed.clone();
            tokio::spawn(async move {
                if cfg!(debug_assertions) {
                    eprintln!("[bgshell][debug] WS 接入新连接");
                }
                let mut stream = stream;
                // 原始流分流（如 /dl HTTP 端点）：消费整条连接；其余不消费字节
                if dispatcher.try_take_stream(&mut stream).await {
                    return;
                }
                handle_conn(stream, dispatcher, allowed).await;
            });
        }
    }
}

async fn handle_conn(
    stream: TcpStream,
    dispatcher: Arc<dyn Dispatcher>,
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
    let outbound = Outbound { tx };

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

    let handler = dispatcher.on_conn_open(outbound).await;

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
                handler.dispatch(frame).await;
            }
            Message::Binary(data) => {
                let Some((frame_type, sid, payload)) = bg_core::protocol::decode_data_frame(&data)
                else {
                    continue;
                };
                handler
                    .dispatch_data(frame_type, &sid, payload.to_vec())
                    .await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    // 连接断开：drop handler 触发连接级清理（会话关闭 / SQL 代际清理等）
}
