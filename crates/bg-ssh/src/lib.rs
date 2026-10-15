//! bg-ssh：SSH 引擎 crate。
//! 连接注册表（pool，多通道复用）/ SSH 建连与 PTY（ssh）/ 会话管理（session）/
//! SFTP 传输与预检（sftp）/ 拖出下载 HTTP 端点（dl_http）/ 远程指标（metrics）。
//! 被 SSH 工作台壳与 AI 基座（远程工具）两个 bin 共用。

pub mod dl_http;
pub mod metrics;
pub mod pool;
pub mod session;
pub mod sftp;
pub mod ssh;
