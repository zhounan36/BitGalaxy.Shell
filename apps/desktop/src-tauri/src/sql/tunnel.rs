//! SSH 隧道桥（direct-tcpip）：Agent 本地 127.0.0.1 随机端口监听，
//! 驱动进程连本地端口 → 每条 TCP 连接开一条 SSH direct-tcpip 通道到远端
//! 数据库端口，双向泵数据。驱动全程只感知 127.0.0.1:port，不感知 SSH。
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::ssh::SshConnection;

const OPEN_DEADLINE: Duration = Duration::from_secs(10);
/// 双向泵空闲超时：数据库连接空闲半小时不发数据视为可断（与驱动空闲回收一致）
const PUMP_IDLE: Duration = Duration::from_secs(1800);

/// direct-tcpip 目标（远端数据库端点）
#[derive(Clone)]
pub struct TunnelTarget {
    pub host: String,
    pub port: u16,
}

/// 一个隧道实例：本地 listener + accept 任务。Drop/stop 时关 listener 并结束 accept。
pub struct Tunnel {
    pub local_port: u16,
    stop: Arc<tokio::sync::Notify>,
}

impl Tunnel {
    pub fn stop(&self) {
        self.stop.notify_waiters();
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop.notify_waiters();
    }
}

/// 启动隧道：绑定 127.0.0.1:0，accept 循环内每条 TCP 连接开一条 direct-tcpip 通道。
/// accept 循环不等待连接处理（各连接独立任务），listener 关闭即退出。
pub async fn open_tunnel(
    conn: Arc<SshConnection>,
    target: TunnelTarget,
) -> Result<Tunnel, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("tunnel_failed: 本地监听失败: {e}"))?;
    let local_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let stop = Arc::new(tokio::sync::Notify::new());
    let stop2 = stop.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop2.notified() => break,
                accepted = listener.accept() => {
                    let Ok((sock, _)) = accepted else { break };
                    let conn = conn.clone();
                    let target = target.clone();
                    tokio::spawn(async move {
                        pump_conn(sock, conn, target).await;
                    });
                }
            }
        }
        // listener 在此 drop 关闭
    });
    Ok(Tunnel { local_port, stop })
}

/// 单条 TCP 连接 → direct-tcpip 通道 → 双向泵（各方向独立 EOF 半关闭）
async fn pump_conn(mut sock: TcpStream, conn: Arc<SshConnection>, target: TunnelTarget) {
    let channel = match tokio::time::timeout(
        OPEN_DEADLINE,
        conn.open_direct_tcpip(&target.host, target.port),
    )
    .await
    {
        Ok(Ok(ch)) => ch,
        // 打开失败：直接掐断本地连接（驱动侧表现为 connection refused/reset）
        _ => {
            let _ = sock.shutdown().await;
            return;
        }
    };
    let (mut ch_read, ch_write) = channel.split();

    let (mut sock_read, mut sock_write) = sock.into_split();

    // 本地 TCP → SSH 通道
    let up = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match tokio::time::timeout(PUMP_IDLE, sock_read.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(n)) => n,
            };
            if ch_write.data(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = ch_write.eof().await;
    };

    // SSH 通道 → 本地 TCP（ChannelMsg::Data / ExtendedData / Eof / Close）
    let down = async {
        loop {
            let msg = match tokio::time::timeout(PUMP_IDLE, ch_read.wait()).await {
                Ok(Some(m)) => m,
                _ => break,
            };
            match msg {
                russh::ChannelMsg::Data { ref data } => {
                    if sock_write.write_all(data).await.is_err() {
                        break;
                    }
                }
                russh::ChannelMsg::ExtendedData { ref data, .. } => {
                    if sock_write.write_all(data).await.is_err() {
                        break;
                    }
                }
                russh::ChannelMsg::Eof | russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = sock_write.shutdown().await;
    };

    // 任一方向结束即整体结束（select 竞争）
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}
