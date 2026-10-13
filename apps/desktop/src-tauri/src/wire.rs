//! 连接内出站通道：SSH 会话 / AI 回包统一经 mpsc 汇聚到 WS 写任务。
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::mpsc;

use crate::protocol::ServerFrame;

#[derive(Debug)]
pub enum OutMsg {
    /// 控制帧（WS 文本帧，JSON）
    Text(String),
    /// 数据帧（WS 二进制帧）
    Binary(Vec<u8>),
}

#[derive(Clone)]
pub struct Outbound {
    pub tx: mpsc::UnboundedSender<OutMsg>,
}

impl Outbound {
    pub fn send_frame(&self, frame: &ServerFrame) {
        if let Ok(text) = serde_json::to_string(frame) {
            let _ = self.tx.send(OutMsg::Text(text));
        }
    }

    pub fn send_binary(&self, data: Vec<u8>) {
        let _ = self.tx.send(OutMsg::Binary(data));
    }
}

/// 进程级广播出站：AI 对话帧发给所有活跃 WS 连接。
/// AiHandler 为进程级单例，而桌宠窗口会断线重连（每次重连是全新连接）；
/// 审批卡片 / 流式回包必须广播才能在重连后的新连接上继续可见，
/// 用户在任何连接上点批准 / 终止也都能路由回同一个审批通道。
#[derive(Clone, Default)]
pub struct OutHub {
    conns: Arc<StdMutex<Vec<Outbound>>>,
}

impl OutHub {
    pub fn register(&self, out: Outbound) {
        self.conns.lock().unwrap().push(out);
    }

    pub fn send_frame(&self, frame: &ServerFrame) {
        let Ok(text) = serde_json::to_string(frame) else { return };
        let mut conns = self.conns.lock().unwrap();
        // 顺带清理已关闭的连接，避免长期运行累积泄漏
        conns.retain(|c| !c.tx.is_closed());
        for c in conns.iter() {
            let _ = c.tx.send(OutMsg::Text(text.clone()));
        }
    }
}
