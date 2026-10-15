//! SSH 会话（russh）：建连（TOFU 指纹校验）/ PTY shell 流 / 一次性 exec / SFTP 子系统。
//! 指纹格式与旧 Node Agent 完全一致（SHA256 + 标准 base64 带 padding），known_hosts 数据互通。
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use russh::client::{Config, Handle, Handler};
use russh::keys::{Algorithm, HashAlg, PrivateKeyWithHashAlg, PublicKey};
use russh::{ChannelMsg, ChannelWriteHalf};
use tokio::sync::oneshot;

use bg_core::protocol::{AuthRetryM, HostKeyInfo, HostRecord, ServerFrame};
use bg_core::stores::KnownHosts;
use bg_core::wire::Outbound;

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
/// 工作台密码重试槽：密码认证被拒后发 auth-retry 帧，挂起等待 retry-auth 回包
///（Some(新密码) = 重试；None = 取消；发送端 drop 视为取消）
pub type AuthSlot = Arc<StdMutex<Option<oneshot::Sender<Option<String>>>>>;

pub enum HostKeyAsk {
    /// 工作台 TOFU：发 hostkey-unknown 帧，等用户在 UI 确认
    Interactive {
        session_id: String,
        out: Outbound,
        slot: ConfirmSlot,
        auth_slot: AuthSlot,
    },
    /// AI/MCP 通道：fail-closed，未知指纹直接拒绝
    FailClosed,
    /// 测试连接（表单「测试连接」按钮）：未知指纹自动接受（不写 known_hosts，
    /// 正式连接仍走 TOFU）；已知指纹不匹配仍拒绝（check_server_key 顶部分支）
    AutoAccept,
}

pub struct HostKeyCtx {
    pub known: KnownHosts,
    pub host: String,
    pub port: u16,
    pub ask: HostKeyAsk,
    /// 指纹不匹配（防中间人）标记，供上层区分错误
    pub mismatch: Arc<std::sync::atomic::AtomicBool>,
    /// 密码重试认证成功且与档案不同时回写主机档案（None = 不回写）
    pub save_password: Option<Arc<dyn Fn(&str) + Send + Sync>>,
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
            HostKeyAsk::AutoAccept => Ok(true),
            HostKeyAsk::Interactive { session_id, out, slot, .. } => {
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
                        ctx.known.save(&bg_core::stores::KnownHostEntry {
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

/// 一条 SSH 连接（连接注册表按主机共享复用）：
/// PTY shell / SFTP / exec / metrics 均为连接上的独立通道，按需打开。
pub struct SshConnection {
    handle: Handle<ConnHandler>,
    pub host_label: String,
}

/// 交互式 PTY shell 通道（从连接上按需打开；每个 shell 会话各自一条）
pub struct PtyShell {
    /// 读半部（stdout/stderr → 上层 take 后转发为数据帧）
    pub reader: StdMutex<Option<russh::ChannelReadHalf>>,
    /// 写半部（stdin / resize）
    write: ChannelWriteHalf<russh::client::Msg>,
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

    // 认证（单次认证交换 15s 超时：半死服务器不能让 open 任务无限挂起；
    // 密码重试等待 UI 输入不受此限制）
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
            match tokio::time::timeout(
                Duration::from_secs(15),
                handle.authenticate_publickey(user, with_hash),
            )
            .await
            {
                Ok(r) => r.map_err(|e| SshError::Other(e.to_string())),
                Err(_) => Err(SshError::Timeout),
            }
        }
        _ => {
            // 密码认证：被拒时工作台交互模式弹窗重试（最多 3 次），AI/MCP 通道直接失败
            let mut password = host.input.auth.password.clone().unwrap_or_default();
            // 档案存储的密码：重试成功后与之不同则回写（下次连接免重输）
            let stored = host.input.auth.password.clone();
            let mut retries_left = 3u8;
            loop {
                let auth_fut =
                    tokio::time::timeout(
                        Duration::from_secs(15),
                        handle.authenticate_password(user.clone(), password.clone()),
                    );
                match auth_fut.await {
                    Ok(Ok(res)) if matches!(res, russh::client::AuthResult::Success) => {
                        if Some(password.as_str()) != stored.as_deref() {
                            if let Some(save) = &ctx.save_password {
                                save(&password);
                            }
                        }
                        break Ok(res)
                    }
                    Ok(Ok(_)) => {
                        if retries_left == 0 {
                            break Err(SshError::Auth(format!(
                                "{}@{}（已重试 3 次）",
                                host.input.username, host.input.host
                            )));
                        }
                        retries_left -= 1;
                        match ask_auth_retry(&ctx, &user, "密码认证被服务器拒绝").await {
                            AuthRetryAnswer::Password(p) => password = p,
                            AuthRetryAnswer::Cancelled => {
                                // 交互模式 = 用户取消重试；AI/MCP/测试连接 = 无重试通道，首拒即败
                                let why = if matches!(ctx.ask, HostKeyAsk::Interactive { .. }) {
                                    "已取消认证"
                                } else {
                                    "密码认证被服务器拒绝"
                                };
                                break Err(SshError::Auth(format!(
                                    "{}@{}: {}",
                                    host.input.username, host.input.host, why
                                )))
                            }
                        }
                    }
                    Ok(Err(e)) => break Err(SshError::Auth(format!("认证中断: {e}"))),
                    Err(_) => break Err(SshError::Timeout),
                }
            }
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
        Err(e) => return Err(e),
    }

    Ok(SshConnection { handle, host_label })
}

pub enum AuthRetryAnswer {
    Password(String),
    Cancelled,
}

/// 密码认证被拒后向工作台请求重输密码：发 auth-retry 帧后挂起等待 retry-auth 回包。
/// 非 Interactive（AI/MCP fail-closed）或等待被取消（会话关闭）时返回 Cancelled。
async fn ask_auth_retry(ctx: &HostKeyCtx, user: &str, message: &str) -> AuthRetryAnswer {
    let HostKeyAsk::Interactive { session_id, out, auth_slot, .. } = &ctx.ask else {
        return AuthRetryAnswer::Cancelled;
    };
    out.send_frame(&ServerFrame::AuthRetry {
        s: session_id.clone(),
        m: AuthRetryM {
            username: user.to_string(),
            host: ctx.host.clone(),
            port: ctx.port,
            message: message.to_string(),
        },
    });
    let (tx, rx) = oneshot::channel::<Option<String>>();
    {
        let mut guard = auth_slot.lock().unwrap();
        *guard = Some(tx);
    }
    // 挂起等待 UI 决定；会话关闭（发送端 drop）视为取消
    match rx.await {
        Ok(Some(p)) => AuthRetryAnswer::Password(p),
        _ => AuthRetryAnswer::Cancelled,
    }
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

    /// 打开交互式 PTY shell 通道（整体 15s 超时，半死连接不让 open 任务挂起）。
    /// 每个调用各自一条通道：同主机多 shell 互不干扰。
    pub async fn open_pty_shell(&self) -> Result<PtyShell, SshError> {
        let setup = async {
            let channel = self
                .handle
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
            Ok::<_, SshError>(channel)
        };
        let channel = match tokio::time::timeout(Duration::from_secs(15), setup).await {
            Ok(Ok(ch)) => ch,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(SshError::Timeout),
        };
        let (read, write) = channel.split();
        Ok(PtyShell {
            reader: StdMutex::new(Some(read)),
            write,
        })
    }

    /// 单次远程命令：超时不抛错，携带已收输出与 timedOut 标志返回
    pub async fn exec(&self, cmd: &str, timeout_ms: u64, max_bytes: usize) -> Result<ExecOutput, String> {
        self.exec_with_stdin(cmd, &[], timeout_ms, max_bytes).await
    }

    /// 同 exec，但启动后先向 stdin 写入一段数据（sudo -S 从 stdin 读密码用），随后发 EOF
    pub async fn exec_with_stdin(
        &self,
        cmd: &str,
        stdin_data: &[u8],
        timeout_ms: u64,
        max_bytes: usize,
    ) -> Result<ExecOutput, String> {
        // 通道打开 + 启动各 10s 超时：事件循环卡死的连接上 channel_open/exec 会永久挂起
        let mut channel = match tokio::time::timeout(
            Duration::from_secs(10),
            self.handle.channel_open_session(),
        )
        .await
        {
            Ok(r) => r.map_err(|e| format!("SSH 连接未建立: {e}"))?,
            Err(_) => return Err("SSH 通道打开超时".into()),
        };
        match tokio::time::timeout(Duration::from_secs(10), channel.exec(true, cmd)).await {
            Ok(r) => r.map_err(|e| e.to_string())?,
            Err(_) => return Err("命令启动超时".into()),
        }
        if !stdin_data.is_empty() {
            let feed = async {
                channel.data(stdin_data).await.map_err(|e| e.to_string())?;
                channel.eof().await.map_err(|e| e.to_string())
            };
            match tokio::time::timeout(Duration::from_secs(10), feed).await {
                Ok(r) => r?,
                Err(_) => return Err("stdin 写入超时".into()),
            }
        }

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

    /// 打开 SFTP 子系统会话（整体 15s 超时）
    pub async fn open_sftp(&self) -> Result<russh_sftp::client::SftpSession, String> {
        let setup = async {
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
        };
        match tokio::time::timeout(Duration::from_secs(15), setup).await {
            Ok(r) => r,
            Err(_) => Err("SFTP 通道打开超时".into()),
        }
    }

    /// 打开 direct-tcpip 通道（SSH 隧道）：连到远端 host:port，10s 超时。
    /// SQL 模块用它把驱动进程的 TCP 流量转发到远端数据库端口。
    pub async fn open_direct_tcpip(
        &self,
        host: &str,
        port: u16,
    ) -> Result<russh::Channel<russh::client::Msg>, SshError> {
        let fut = self.handle.channel_open_direct_tcpip(host, port as u32, "127.0.0.1", 0);
        match tokio::time::timeout(Duration::from_secs(10), fut).await {
            Ok(Ok(ch)) => Ok(ch),
            Ok(Err(e)) => Err(SshError::Other(format!("隧道通道打开失败: {e}"))),
            Err(_) => Err(SshError::Timeout),
        }
    }

    pub async fn close(&self) {
        let _ = self.handle.disconnect(russh::Disconnect::ByApplication, "close", "en");
    }
}

impl PtyShell {
    /// PTY stdin（5s 写超时：僵尸连接的流控窗口打满时 data() 会无限挂起，
    /// 快速失败避免上层写入任务堆积）
    pub async fn write_input(&self, data: &[u8]) -> Result<(), String> {
        match tokio::time::timeout(Duration::from_secs(5), self.write.data(data)).await {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err("写入超时（连接可能已断开）".into()),
        }
    }

    /// PTY 尺寸（同 write_input 的 5s 写超时）
    pub async fn resize(&self, cols: u32, rows: u32) -> Result<(), String> {
        match tokio::time::timeout(
            Duration::from_secs(5),
            self.write.window_change(cols, rows, 480, 240),
        )
        .await
        {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err("写入超时（连接可能已断开）".into()),
        }
    }
}

/// exec 输出聚合（AI 工具层复用，故 pub）
pub fn collect_text(buf: &mut Vec<u8>, chunk: &[u8], max_bytes: usize, truncated: &mut bool) {
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
