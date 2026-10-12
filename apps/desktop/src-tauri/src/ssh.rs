//! SSH 会话（russh）：建连（TOFU 指纹校验）/ PTY shell 流 / 一次性 exec / SFTP 子系统。
//! 指纹格式与旧 Node Agent 完全一致（SHA256 + 标准 base64 带 padding），known_hosts 数据互通。
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use russh::client::{Config, Handle, Handler};
use russh::keys::{Algorithm, HashAlg, PrivateKeyWithHashAlg, PublicKey};
use russh::{ChannelMsg, ChannelWriteHalf};
use tokio::sync::oneshot;

use crate::protocol::{HostKeyInfo, HostRecord, ServerFrame};
use crate::stores::KnownHosts;
use crate::wire::Outbound;

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("认证失败（{0}）")]
    Auth(String),
    #[error("无法连接 {host}:{port}: {msg}")]
    Connect { host: String, port: u16, msg: String },
    #[error("连接超时")]
    Timeout,
    #[error("主机指纹校验未通过")]
    HostKeyRejected,
    #[error("{0}")]
    Other(String),
}

impl SshError {
    pub fn message(&self) -> String {
        self.to_string()
    }
}

// ── host key 校验上下文 ───────────────────────────────────────────────────

/// 工作台交互确认槽：check_server_key 发帧后挂起等待 confirm-hostkey 回包
pub type ConfirmSlot = Arc<StdMutex<Option<oneshot::Sender<bool>>>>;

pub enum HostKeyAsk {
    /// 工作台 TOFU：发 hostkey-unknown 帧，等用户在 UI 确认
    Interactive {
        session_id: String,
        out: Outbound,
        slot: ConfirmSlot,
    },
    /// AI/MCP 通道：fail-closed，未知指纹直接拒绝
    FailClosed,
}

pub struct HostKeyCtx {
    pub known: KnownHosts,
    pub host: String,
    pub port: u16,
    pub ask: HostKeyAsk,
    /// 指纹不匹配（防中间人）标记，供上层区分错误
    pub mismatch: Arc<std::sync::atomic::AtomicBool>,
}

struct ConnHandler {
    ctx: Arc<HostKeyCtx>,
}

impl Handler for ConnHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, server_public_key: &PublicKey) -> Result<bool, Self::Error> {
        let ctx = &self.ctx;
        let fingerprint = ssh_key_fingerprint(server_public_key);
        let key_type = server_public_key.algorithm().as_str().to_string();

        if let Some(known) = ctx.known.find(&ctx.host, ctx.port) {
            if known.fingerprint == fingerprint {
                return Ok(true);
            }
            ctx.mismatch.store(true, std::sync::atomic::Ordering::Relaxed);
            if let HostKeyAsk::Interactive { out, .. } = &ctx.ask {
                out.send_frame(&ServerFrame::error(
                    None,
                    "ssh-error",
                    format!("主机指纹不匹配（疑似中间人攻击）：{}:{}", ctx.host, ctx.port),
                ));
            }
            return Ok(false);
        }

        match &ctx.ask {
            HostKeyAsk::FailClosed => Ok(false),
            HostKeyAsk::Interactive { session_id, out, slot } => {
                out.send_frame(&ServerFrame::HostKeyUnknown {
                    s: session_id.clone(),
                    m: HostKeyInfo {
                        host: ctx.host.clone(),
                        port: ctx.port,
                        key_type: key_type.clone(),
                        fingerprint: fingerprint.clone(),
                    },
                });
                let (tx, rx) = oneshot::channel::<bool>();
                {
                    let mut guard = slot.lock().unwrap();
                    *guard = Some(tx);
                }
                // 挂起等待 UI 决定；会话关闭（发送端 drop）视为拒绝
                match rx.await {
                    Ok(true) => {
                        ctx.known.save(&crate::stores::KnownHostEntry {
                            host: ctx.host.clone(),
                            port: ctx.port,
                            key_type,
                            fingerprint,
                        });
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
        }
    }
}

/// 主机公钥指纹：SHA256(SSH wire blob) + 标准 base64（带 padding）。
/// 直接对 public_key_base64 解码出的 wire blob 自行哈希——与旧 Node Agent
/// （ssh2 hostVerifier 的 key blob + createHash）逐字节一致，known_hosts 数据互通。
fn ssh_key_fingerprint(pk: &PublicKey) -> String {
    use base64::Engine;
    use russh::keys::PublicKeyBase64;
    use sha2::{Digest, Sha256};
    let engine = base64::engine::general_purpose::STANDARD;
    let blob = engine.decode(pk.public_key_base64()).unwrap_or_default();
    format!("SHA256:{}", engine.encode(Sha256::digest(blob)))
}

// ── 连接 ──────────────────────────────────────────────────────────────────

pub enum SshKind {
    Shell,
    Sftp,
    Metrics,
    Exec,
}

pub struct SshConnection {
    handle: Handle<ConnHandler>,
    /// shell 会话的写半部（stdin / resize）
    pty: Option<ChannelWriteHalf<russh::client::Msg>>,
    /// shell 会话的读半部（stdout/stderr → 上层 take 后转发为数据帧）
    pub shell_reader: StdMutex<Option<russh::ChannelReadHalf>>,
    pub host_label: String,
}

pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub truncated: bool,
    pub timed_out: bool,
}

pub async fn connect(
    host: &HostRecord,
    kind: &SshKind,
    ctx: Arc<HostKeyCtx>,
) -> Result<SshConnection, SshError> {
    let config = Arc::new(Config {
        inactivity_timeout: Some(Duration::from_secs(75)),
        keepalive_interval: Some(Duration::from_secs(15)),
        ..Default::default()
    });
    let addr = (host.input.host.clone(), host.input.port);

    let mut handle = {
        let connect_fut = russh::client::connect(config, addr, ConnHandler { ctx: ctx.clone() });
        match tokio::time::timeout(Duration::from_secs(15), connect_fut).await {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                return Err(classify_connect(e, &host.input.host, host.input.port));
            }
            Err(_) => return Err(SshError::Timeout),
        }
    };

    let host_label = format!("{}@{}:{}", host.input.username, host.input.host, host.input.port);

    // 认证
    let user = host.input.username.clone();
    let auth_ok = match host.input.auth.kind.as_str() {
        "privateKey" => {
            let path = host
                .input
                .auth
                .private_key_path
                .as_deref()
                .ok_or_else(|| SshError::Other("私钥认证缺少 privateKeyPath".into()))?;
            let secret = std::fs::read_to_string(path)
                .map_err(|e| SshError::Other(format!("读取私钥失败 {path}: {e}")))?;
            let passphrase = host.input.auth.passphrase.as_deref();
            let key = russh::keys::decode_secret_key(&secret, passphrase)
                .map_err(|e| SshError::Auth(format!("私钥解析失败: {e}")))?;
            // RSA 按 rsa-sha2-512 签名（OpenSSH 7.2+ 支持；旧服务器见 HashAlg::Sha1 可按需扩展）
            let hash_alg = if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
                Some(HashAlg::Sha512)
            } else {
                None
            };
            let with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
            handle.authenticate_publickey(user, with_hash).await
        }
        _ => {
            let password = host.input.auth.password.clone().unwrap_or_default();
            handle.authenticate_password(user, password).await
        }
    };
    match auth_ok {
        Ok(res) if matches!(res, russh::client::AuthResult::Success) => {}
        Ok(_) => {
            return Err(SshError::Auth(format!(
                "{}@{}",
                host.input.username, host.input.host
            )))
        }
        Err(e) => return Err(SshError::Auth(e.to_string())),
    }

    // shell 会话：打开 PTY + shell；其余 kind 建连即返回
    let mut pty = None;
    let mut shell_reader = None;
    if matches!(kind, SshKind::Shell) {
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::Other(e.to_string()))?;
        channel
            .request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
            .await
            .map_err(|e| SshError::Other(e.to_string()))?;
        channel
            .request_shell(true)
            .await
            .map_err(|e| SshError::Other(e.to_string()))?;
        let (read, write) = channel.split();
        pty = Some(write);
        shell_reader = Some(read);
    }
    Ok(SshConnection {
        handle,
        pty,
        shell_reader: StdMutex::new(shell_reader),
        host_label,
    })
}

fn classify_connect(e: russh::Error, host: &str, port: u16) -> SshError {
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        SshError::Timeout
    } else {
        SshError::Connect {
            host: host.to_string(),
            port,
            msg,
        }
    }
}

impl SshConnection {
    pub fn is_alive(&self) -> bool {
        !self.handle.is_closed()
    }

    /// PTY stdin
    pub async fn write_input(&self, data: &[u8]) -> Result<(), String> {
        if let Some(pty) = &self.pty {
            pty.data(data).await.map_err(|e| e.to_string())
        } else {
            Ok(())
        }
    }

    /// PTY 尺寸
    pub async fn resize(&self, cols: u32, rows: u32) -> Result<(), String> {
        if let Some(pty) = &self.pty {
            pty.window_change(cols, rows, 480, 240).await.map_err(|e| e.to_string())
        } else {
            Ok(())
        }
    }

    /// 单次远程命令：超时不抛错，携带已收输出与 timedOut 标志返回
    pub async fn exec(&self, cmd: &str, timeout_ms: u64, max_bytes: usize) -> Result<ExecOutput, String> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| format!("SSH 连接未建立: {e}"))?;
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| e.to_string())?;

        let mut stdout_buf: Vec<u8> = Vec::new();
        let mut stderr_buf: Vec<u8> = Vec::new();
        let mut truncated = false;
        let mut code: Option<i32> = None;

        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let msg = tokio::select! {
                m = channel.wait() => m,
                _ = tokio::time::sleep_until(deadline) => {
                    return Ok(ExecOutput { stdout: to_utf8_lossy(&stdout_buf), stderr: to_utf8_lossy(&stderr_buf), code: None, truncated, timed_out: true });
                }
            };
            let Some(msg) = msg else { break };
            match msg {
                ChannelMsg::Data { ref data } => collect_text(&mut stdout_buf, data, max_bytes, &mut truncated),
                ChannelMsg::ExtendedData { ref data, .. } => collect_text(&mut stderr_buf, data, max_bytes, &mut truncated),
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status as i32),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(ExecOutput {
            stdout: to_utf8_lossy(&stdout_buf),
            stderr: to_utf8_lossy(&stderr_buf),
            code,
            truncated,
            timed_out: false,
        })
    }

    /// 打开 SFTP 子系统会话
    pub async fn open_sftp(&self) -> Result<russh_sftp::client::SftpSession, String> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| format!("SSH 连接未建立: {e}"))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| e.to_string())?;
        russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| format!("SFTP 通道打开失败: {e}"))
    }

    pub async fn close(&self) {
        let _ = self.handle.disconnect(russh::Disconnect::ByApplication, "close", "en");
    }
}

fn collect_text(buf: &mut Vec<u8>, chunk: &[u8], max_bytes: usize, truncated: &mut bool) {
    if buf.len() >= max_bytes {
        *truncated = true;
        return;
    }
    let room = max_bytes - buf.len();
    if chunk.len() > room {
        buf.extend_from_slice(&chunk[..room]);
        *truncated = true;
    } else {
        buf.extend_from_slice(chunk);
    }
}

fn to_utf8_lossy(buf: &[u8]) -> String {
    String::from_utf8_lossy(buf).into_owned()
}
