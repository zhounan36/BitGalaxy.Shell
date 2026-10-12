//! 连接内出站通道：SSH 会话 / AI 回包统一经 mpsc 汇聚到 WS 写任务。
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
