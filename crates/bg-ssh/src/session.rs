//! 会话管理器：一个 WS 连接对应一个实例。
//! 三种会话 kind（shell/sftp/metrics）经统一连接注册表（[`crate::pool::ConnRegistry`]）
//! 获取同主机共享的 SSH 连接：shell 各自开 PTY 通道，sftp/metrics 复用池连接；
//! 会话关闭 release 持有者，连接由注册表在无引用后按空闲超时回收。
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::metrics;
use crate::pool::{ConnRegistry, CreateCtx};
use crate::sftp::SftpController;
use crate::ssh::{AuthSlot, ConfirmSlot, HostKeyAsk, PtyShell, SshConnection};
use bg_core::protocol::{
    self, HostsResultM, ProbeResultM, ServerFrame, SessionOpenedM, SftpResultM, DF_STDOUT,
};
use bg_core::stores::Stores;
use bg_core::wire::Outbound;

type SessionMap = Arc<StdMutex<HashMap<String, SessionEntry>>>;

struct SessionEntry {
    kind: String,
    /// 连接注册表持有者键（release 用）
    host_id: Option<String>,
    conn: Option<Arc<SshConnection>>,
    /// shell 会话的 PTY 通道（stdin/resize；与连接分离，同主机多 shell 各自独立）
    pty: Option<Arc<PtyShell>>,
    sftp: Option<Arc<SftpController>>,
    metrics_task: Option<JoinHandle<()>>,
    /// 建连期间到达的 metrics 订阅，就绪后自动启动
    metrics_pending: Option<i64>,
    confirm_slot: ConfirmSlot,
    /// 密码认证失败重试：connect 挂起等 retry-auth 回包
    auth_slot: AuthSlot,
    /// stdin 写入队列：独立任务按序写 PTY。僵尸连接上 pty.data() 会挂起，
    /// 若在 WS 分发循环里直接 await 会连带阻塞所有后续帧（open/probe 全部失响应）
    stdin_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    /// 建连期间到达的终端尺寸：PTY 未就绪时 resize 帧先缓存在此，
    /// open 完成后补发，避免远端停留在 open 帧占位 80x24（与前端 xterm
    /// 实际尺寸不一致 → readline 光标/折行错位，历史被覆盖遮挡）
    pending_size: Option<(u32, u32)>,
}

impl SessionEntry {
    fn new(kind: &str) -> SessionEntry {
        SessionEntry {
            kind: kind.to_string(),
            host_id: None,
            conn: None,
            pty: None,
            sftp: None,
            metrics_task: None,
            metrics_pending: None,
            confirm_slot: Arc::new(StdMutex::new(None)),
            auth_slot: Arc::new(StdMutex::new(None)),
            stdin_tx: None,
            pending_size: None,
        }
    }
}

#[derive(Clone)]
pub struct SessionManager {
    stores: Stores,
    out: Outbound,
    registry: Arc<ConnRegistry>,
    sessions: SessionMap,
    /// 拖出下载能力令牌表（dl-open 签发；HTTP 端点消费）
    dl: Arc<crate::dl_http::DlTickets>,
}

impl SessionManager {
    pub fn new(
        stores: Stores,
        out: Outbound,
        registry: Arc<ConnRegistry>,
        dl: Arc<crate::dl_http::DlTickets>,
    ) -> SessionManager {
        SessionManager {
            stores,
            out,
            registry,
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            dl,
        }
    }

    fn send_control(&self, frame: ServerFrame) {
        self.out.send_frame(&frame);
    }

    pub fn open(&self, session_id: &str, host_id: &str, opts: &protocol::OpenOptions) {
        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.contains_key(session_id) {
                return;
            }
            let kind = opts.kind.clone().unwrap_or_else(|| "shell".into());
            sessions.insert(session_id.to_string(), SessionEntry::new(&kind));
        }

        let this = self.clone();
        let session_id = session_id.to_string();
        let host_id = host_id.to_string();
        let kind = opts.kind.clone().unwrap_or_else(|| "shell".into());
        let cols = opts.cols.unwrap_or(80);
        let rows = opts.rows.unwrap_or(24);
        tokio::spawn(async move {
            let Some(host) = this.stores.hosts.get(&host_id) else {
                this.fail_open(&session_id, "主机档案不存在");
                return;
            };

            let slots = {
                let sessions = this.sessions.lock().unwrap();
                sessions
                    .get(&session_id)
                    .map(|e| (e.confirm_slot.clone(), e.auth_slot.clone()))
            };
            let Some((confirm_slot, auth_slot)) = slots else { return };

            let create = CreateCtx {
                ask: HostKeyAsk::Interactive {
                    session_id: session_id.clone(),
                    out: this.out.clone(),
                    slot: confirm_slot,
                    auth_slot,
                },
                initiator: "workbench".into(),
            };

            // 统一连接注册表：同主机共享一条 TCP 连接（TOFU/密码重试走本会话弹窗）
            let conn = match this.registry.acquire_session(&host_id, &session_id, create).await {
                Ok(c) => c,
                Err(e) => {
                    let reason = if e.code == bg_core::error::ERR_HOSTKEY_MISMATCH {
                        "主机指纹校验未通过（疑似中间人攻击）".to_string()
                    } else {
                        e.message
                    };
                    this.fail_open(&session_id, &reason);
                    return;
                }
            };

            // shell：在共享连接上开独立 PTY 通道，接管读半部转发为 0x01 数据帧；
            // 流结束 → 会话关闭
            let mut pty = None;
            if ssh_kind_matches(&kind, "shell") {
                match conn.open_pty_shell().await {
                    Ok(p) => {
                        let reader = p.reader.lock().unwrap().take();
                        if let Some(mut reader) = reader {
                            let this2 = this.clone();
                            let sid = session_id.clone();
                            tokio::spawn(async move {
                                loop {
                                    let msg = reader.wait().await;
                                    match msg {
                                        Some(russh::ChannelMsg::Data { ref data }) => {
                                            this2.out.send_binary(protocol::encode_data_frame(
                                                DF_STDOUT,
                                                &sid,
                                                data,
                                            ));
                                        }
                                        Some(russh::ChannelMsg::ExtendedData { ref data, .. }) => {
                                            this2.out.send_binary(protocol::encode_data_frame(
                                                DF_STDOUT,
                                                &sid,
                                                data,
                                            ));
                                        }
                                        Some(_) => {}
                                        None => {
                                            this2.handle_closed(&sid, "shell-closed");
                                            return;
                                        }
                                    }
                                }
                            });
                        }
                        // 记录初始 PTY 尺寸：优先取建连期间缓存的 resize
                        //（前端 TerminalPane 挂载即上报真实尺寸，往往先于建连完成）
                        let init_size = {
                            let mut sessions = this.sessions.lock().unwrap();
                            sessions
                                .get_mut(&session_id)
                                .and_then(|e| e.pending_size.take())
                                .unwrap_or((cols, rows))
                        };
                        let _ = p.resize(init_size.0, init_size.1).await;
                        pty = Some(Arc::new(p));
                    }
                    Err(e) => {
                        // 持有者已注册但 PTY 失败：先释放再报错
                        this.registry.release(&host_id, &session_id);
                        this.fail_open(&session_id, &e.message());
                        return;
                    }
                }
            }

            // sftp：复用注册表缓存的共享 SFTP 通道（同主机与 AI/其他会话共用）
            let sftp = if ssh_kind_matches(&kind, "sftp") {
                match this.registry.shared_sftp(&host_id).await {
                    Ok(s) => Some(Arc::new(SftpController::from_shared(s))),
                    Err(e) => {
                        this.registry.release(&host_id, &session_id);
                        this.fail_open(&session_id, &e.message);
                        return;
                    }
                }
            } else {
                None
            };

            // 连接断开监视（所有 kind）：russh 检测到断开（keepalive 超时/对端断开）而
            // shell reader 未触发时，由轮询兜底关闭会话并提示，避免「无提示假死」
            {
                let this2 = this.clone();
                let sid = session_id.clone();
                let conn2 = conn.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        if !conn2.is_alive() {
                            this2.handle_closed(&sid, "connection-lost");
                            return;
                        }
                    }
                });
            }

            // stdin 专用写入任务：按序消费队列写 PTY；写失败（超时/连接断开）即退出，
            // 会话清理时 sender 被 drop 自然结束
            let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
            if let Some(pty_w) = pty.clone() {
                tokio::spawn(async move {
                    while let Some(data) = stdin_rx.recv().await {
                        if pty_w.write_input(&data).await.is_err() {
                            break;
                        }
                    }
                });
            }

            let (pending_metrics, leftover_size) = {
                let mut sessions = this.sessions.lock().unwrap();
                let Some(entry) = sessions.get_mut(&session_id) else {
                    // 会话在建连期间被用户关闭：释放持有者，连接交还注册表
                    drop(sessions);
                    this.registry.release(&host_id, &session_id);
                    return;
                };
                entry.host_id = Some(host_id.clone());
                entry.conn = Some(conn.clone());
                entry.pty = pty.clone();
                entry.sftp = sftp.clone();
                if pty.is_some() {
                    entry.stdin_tx = Some(stdin_tx);
                }
                (entry.metrics_pending.take(), entry.pending_size.take())
            };

            // 补发窗口期到达的 resize：从初始尺寸计算到 entry.pty 登记之间
            // 到达的帧只被缓存不消费，PTY 就绪登记后再补一次（此后走直达路径）
            if let (Some(pty), Some((c, r))) = (pty.as_ref(), leftover_size) {
                let _ = pty.resize(c, r).await;
            }

            this.send_control(ServerFrame::SessionOpened {
                s: session_id.clone(),
                m: SessionOpenedM {
                    host_id: host_id.clone(),
                    kind: kind.clone(),
                    dtr: None,
                    rts: None,
                },
            });
            this.stores.audit.append(
                "session.opened",
                serde_json::json!({
                    "sessionId": session_id,
                    "hostId": host_id,
                    "kind": kind,
                    "target": format!("{}@{}:{}", host.input.username, host.input.host, host.input.port),
                }),
            );

            if ssh_kind_matches(&kind, "metrics") {
                if let Some(sec) = pending_metrics {
                    this.start_metrics(&session_id, sec, &conn);
                }
            }
        });
    }

    fn fail_open(&self, session_id: &str, reason: &str) {
        self.send_control(ServerFrame::error(Some(session_id), "open-failed", reason.to_string()));
        self.handle_closed(session_id, &format!("open-failed: {reason}"));
    }

    // ── terminal ───────────────────────────────────────────────────────────

    /// 终端输入（DF_STDIN）：命中 SSH 会话入队即返回 true；
    /// 未命中返回 false（调用方回落串口会话表等其他会话类型）
    pub fn try_input(&self, session_id: &str, data: &[u8]) -> bool {
        let tx = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).and_then(|e| e.stdin_tx.clone())
        };
        // 入队即返回：实际写入由会话的 stdin 任务执行（写挂起不影响 WS 分发循环）
        if let Some(tx) = tx {
            let _ = tx.send(data.to_vec());
            true
        } else {
            false
        }
    }

    pub async fn resize(&self, session_id: &str, cols: u32, rows: u32) {
        let pty = {
            let mut sessions = self.sessions.lock().unwrap();
            match sessions.get_mut(session_id) {
                Some(e) if e.pty.is_some() => e.pty.clone(),
                // PTY 未就绪（open 建连中）：缓存尺寸，open 完成后补发。
                // 此前直接丢弃会让远端永远停留在占位 80x24，与前端实际
                // 尺寸不一致 → 方向键调历史时 readline 折行/光标错位，
                // 表现为历史被覆盖遮挡、长句截断、字符重叠
                Some(e) => {
                    e.pending_size = Some((cols, rows));
                    None
                }
                None => None,
            }
        };
        if let Some(pty) = pty {
            let _ = pty.resize(cols, rows).await;
        }
    }

    // ── sftp ───────────────────────────────────────────────────────────────

    pub fn handle_sftp(&self, session_id: &str, op: &protocol::SftpOpM) {
        let (sftp, conn, host_id) = {
            let sessions = self.sessions.lock().unwrap();
            let Some(entry) = sessions.get(session_id) else {
                self.send_control(ServerFrame::SftpResult {
                    s: session_id.to_string(),
                    m: err_sftp(&op.op, Some(op.rid.clone()), "会话未就绪"),
                });
                return;
            };
            (entry.sftp.clone(), entry.conn.clone(), entry.host_id.clone())
        };

        // dl-open：只依赖会话的 host_id（签发拖出下载能力令牌），无需 sftp 控制器，
        // 在入口同步处理即可
        if op.op == "dl-open" {
            let m = match host_id.as_deref() {
                Some(hid) => ok_sftp("dl-open", Some(op.rid.clone()), |m| {
                    m.dl_base = Some(self.dl.issue(hid));
                }),
                None => err_sftp("dl-open", Some(op.rid.clone()), "会话未就绪"),
            };
            self.send_control(ServerFrame::SftpResult {
                s: session_id.to_string(),
                m,
            });
            return;
        }

        let op_clone = op.clone();
        let session_id = session_id.to_string();
        let this = self.clone();
        let out = self.out.clone();
        tokio::spawn(async move {
            // pwd：在 shell 会话的连接上开独立 exec 通道取目录与主机名（登录目录；
            // 交互式 cd 后的实时目录由前端 OSC 7 上报。主机名供前端校验 OSC 7
            // 是否来自本连接的主机——嵌套 SSH 内层 shell 的上报会被拒绝）
            if op_clone.op == "pwd" {
                let m = match conn {
                    Some(conn) => match conn.exec("pwd; hostname", 8_000, 4_096).await {
                        Ok(r) if !r.timed_out => {
                            let mut lines = r.stdout.lines();
                            let dir = lines.next().unwrap_or_default().trim().to_string();
                            let host = lines.next().unwrap_or_default().trim().to_string();
                            if dir.is_empty() {
                                err_sftp("pwd", Some(op_clone.rid.clone()), "获取远程目录失败")
                            } else {
                                ok_sftp("pwd", Some(op_clone.rid.clone()), |m| {
                                    m.resolved_path = Some(dir);
                                    // hostname 不可用（极简容器等）→ 缺省，前端跳过校验
                                    if !host.is_empty() {
                                        m.host = Some(host);
                                    }
                                })
                            }
                        }
                        Ok(_) => err_sftp("pwd", Some(op_clone.rid.clone()), "获取远程目录超时"),
                        Err(e) => err_sftp("pwd", Some(op_clone.rid.clone()), &e),
                    },
                    None => err_sftp("pwd", Some(op_clone.rid.clone()), "会话未就绪"),
                };
                out.send_frame(&ServerFrame::SftpResult { s: session_id, m });
                return;
            }

            // SFTP 控制器：sftp 会话建连时已挂载；shell 会话（终端拖拽上传）在此惰性
            // 取注册表缓存的共享 SFTP 通道（同主机所有使用方复用同一条）
            let sftp = match sftp {
                Some(s) => s,
                None => {
                    let Some(hid) = host_id.as_deref() else {
                        out.send_frame(&ServerFrame::SftpResult {
                            s: session_id.clone(),
                            m: err_sftp(&op_clone.op, Some(op_clone.rid.clone()), "会话未就绪"),
                        });
                        return;
                    };
                    match this.registry.shared_sftp(hid).await {
                        Ok(s) => {
                            let ctrl = Arc::new(SftpController::from_shared(s));
                            // 双检：并发首个完成者生效；会话已关闭则放弃
                            let mut sessions = this.sessions.lock().unwrap();
                            match sessions.get_mut(&session_id) {
                                Some(entry) if entry.sftp.is_none() => {
                                    entry.sftp = Some(ctrl.clone());
                                    ctrl
                                }
                                Some(entry) => entry.sftp.clone().unwrap_or(ctrl),
                                None => return,
                            }
                        }
                        Err(e) => {
                            out.send_frame(&ServerFrame::SftpResult {
                                s: session_id.clone(),
                                m: err_sftp(&op_clone.op, Some(op_clone.rid.clone()), &e.message),
                            });
                            return;
                        }
                    }
                }
            };

            let m = match op_clone.op.as_str() {
                "ls" => match sftp.ls(op_clone.path.as_deref().unwrap_or(".")).await {
                    Ok((resolved, entries)) => ok_sftp("ls", Some(op_clone.rid.clone()), |m| {
                        m.resolved_path = Some(resolved);
                        m.entries = Some(entries);
                    }),
                    Err(e) => err_sftp("ls", Some(op_clone.rid.clone()), &e),
                },
                "mkdir" => match sftp.mkdir(op_clone.path.as_deref().unwrap_or_default()).await {
                    Ok(()) => ok_sftp("mkdir", Some(op_clone.rid.clone()), |_| {}),
                    Err(e) => err_sftp("mkdir", Some(op_clone.rid.clone()), &e),
                },
                "rm" => match sftp
                    .rm(
                        op_clone.path.as_deref().unwrap_or_default(),
                        op_clone.recursive.unwrap_or(false),
                    )
                    .await
                {
                    Ok(()) => ok_sftp("rm", Some(op_clone.rid.clone()), |_| {}),
                    Err(e) => err_sftp("rm", Some(op_clone.rid.clone()), &e),
                },
                "rename" => match sftp
                    .rename(
                        op_clone.from.as_deref().unwrap_or_default(),
                        op_clone.to.as_deref().unwrap_or_default(),
                    )
                    .await
                {
                    Ok(()) => ok_sftp("rename", Some(op_clone.rid.clone()), |_| {}),
                    Err(e) => err_sftp("rename", Some(op_clone.rid.clone()), &e),
                },
                "upload-open" => {
                    match sftp
                        .upload_open(op_clone.path.as_deref().unwrap_or_default())
                        .await
                    {
                        Ok(op_id) => ok_sftp("upload-open", Some(op_clone.rid.clone()), |m| {
                            m.op_id = Some(op_id);
                        }),
                        // create 被拒（无写权限）/死连接在此即刻失败，前端不必
                        // 推完全部块才在 upload-close 发现
                        Err(e) => err_sftp("upload-open", Some(op_clone.rid.clone()), &e),
                    }
                }
                "upload-close" => match sftp
                    .upload_close(op_clone.op_id.as_deref().unwrap_or_default())
                    .await
                {
                    Ok(()) => ok_sftp("upload-close", Some(op_clone.rid.clone()), |_| {}),
                    Err(e) => err_sftp("upload-close", Some(op_clone.rid.clone()), &e),
                },
                "download-open" => {
                    match sftp
                        .download_open(
                            &session_id,
                            op_clone.path.as_deref().unwrap_or_default(),
                            &out,
                        )
                        .await
                    {
                        Ok((op_id, size)) => ok_sftp("download-open", Some(op_clone.rid.clone()), |m| {
                            m.op_id = Some(op_id);
                            m.size_b = Some(size);
                        }),
                        Err(e) => err_sftp("download-open", Some(op_clone.rid.clone()), &e),
                    }
                }
                "download-close" => {
                    sftp.download_close(op_clone.op_id.as_deref().unwrap_or_default());
                    ok_sftp("download-close", Some(op_clone.rid.clone()), |_| {})
                }
                // ── 传输预检：重名/权限/剩余空间/目标盘格式（前端与 AI 共用）──
                "preflight" => {
                    match require_paths(op_clone.path.as_deref(), op_clone.to.as_deref()) {
                        Ok((path, to)) => {
                            let direction = op_clone.direction.as_deref().unwrap_or("upload");
                            // 内部多为远端元数据操作：整体 15s 兜底，半开死连接快速
                            // 失败（"timeout" → network → 下方统一驱逐），权限探针毫秒级
                            match tokio::time::timeout(
                                crate::sftp::SFTP_META_DEADLINE,
                                build_preflight_report(
                                    &sftp,
                                    direction,
                                    path,
                                    to,
                                    op_clone.size_b,
                                ),
                            )
                            .await
                            {
                                Ok(Ok(report)) => {
                                    ok_sftp("preflight", Some(op_clone.rid.clone()), |m| {
                                        m.preflight = Some(report);
                                    })
                                }
                                Ok(Err(e)) => err_sftp("preflight", Some(op_clone.rid.clone()), &e),
                                Err(_) => err_sftp(
                                    "preflight",
                                    Some(op_clone.rid.clone()),
                                    "preflight timeout (15s)",
                                ),
                            }
                        }
                        Err(e) => err_sftp("preflight", Some(op_clone.rid.clone()), e),
                    }
                }
                // ── 原生传输（Agent 直传 + 断点续传，进度/结束经 sftp-progress/transfer-done）──
                "get-file" => match require_paths(
                    op_clone.path.as_deref(),
                    op_clone.to.as_deref(),
                ) {
                    Ok((remote, local)) => {
                        let op_id = sftp.start_get_file(
                            &session_id,
                            remote,
                            local,
                            &out,
                            op_clone.fresh.unwrap_or(false),
                        );
                        ok_sftp("get-file", Some(op_clone.rid.clone()), |m| {
                            m.op_id = Some(op_id);
                        })
                    }
                    Err(e) => err_sftp("get-file", Some(op_clone.rid.clone()), e),
                },
                "get-dir" => match require_paths(
                    op_clone.path.as_deref(),
                    op_clone.to.as_deref(),
                ) {
                    Ok((remote_dir, local_dir)) => {
                        let op_id = sftp.start_get_dir(
                            &session_id,
                            remote_dir,
                            local_dir,
                            &out,
                            op_clone.fresh.unwrap_or(false),
                        );
                        ok_sftp("get-dir", Some(op_clone.rid.clone()), |m| {
                            m.op_id = Some(op_id);
                        })
                    }
                    Err(e) => err_sftp("get-dir", Some(op_clone.rid.clone()), e),
                },
                "put-file" => match require_paths(
                    op_clone.path.as_deref(),
                    op_clone.to.as_deref(),
                ) {
                    Ok((local, remote)) => {
                        let op_id = sftp.start_put_file(
                            &session_id,
                            local,
                            remote,
                            &out,
                            op_clone.fresh.unwrap_or(false),
                        );
                        ok_sftp("put-file", Some(op_clone.rid.clone()), |m| {
                            m.op_id = Some(op_id);
                        })
                    }
                    Err(e) => err_sftp("put-file", Some(op_clone.rid.clone()), e),
                },
                "put-dir" => match require_paths(
                    op_clone.path.as_deref(),
                    op_clone.to.as_deref(),
                ) {
                    Ok((local_dir, remote_dir)) => {
                        let op_id = sftp.start_put_dir(
                            &session_id,
                            local_dir,
                            remote_dir,
                            &out,
                            op_clone.fresh.unwrap_or(false),
                        );
                        ok_sftp("put-dir", Some(op_clone.rid.clone()), |m| {
                            m.op_id = Some(op_id);
                        })
                    }
                    Err(e) => err_sftp("put-dir", Some(op_clone.rid.clone()), e),
                },
                "transfer-cancel" => {
                    let cancelled =
                        sftp.transfer_cancel(op_clone.op_id.as_deref().unwrap_or_default());
                    ok_sftp("transfer-cancel", Some(op_clone.rid.clone()), |m| {
                        if !cancelled {
                            m.message = Some("任务不存在或已结束".into());
                        }
                    })
                }
                other => err_sftp(other, Some(op_clone.rid.clone()), "未知 SFTP 操作"),
            };
            // 半开死连接快速恢复：网络类失败（含 15s 操作超时）→ 驱逐池条目
            // （各会话看门狗 5s 内感知断开并提示重连），避免死连接被反复复用、
            // 每个操作都白等一遍超时。权限被拒（permission_denied）不在此列
            if !m.ok && m.error_kind.as_deref() == Some("network") {
                if let Some(hid) = host_id.as_deref() {
                    this.registry.evict(hid, "connection-error");
                }
            }
            out.send_frame(&ServerFrame::SftpResult {
                s: session_id,
                m,
            });
        });
    }

    pub fn upload_chunk(&self, session_id: &str, op_id: &str, chunk: Vec<u8>) {
        let sftp = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).and_then(|e| e.sftp.clone())
        };
        if let Some(sftp) = sftp {
            sftp.upload_chunk(op_id, chunk);
        }
    }

    // ── metrics ────────────────────────────────────────────────────────────

    pub fn set_metrics_interval(&self, session_id: &str, interval_sec: i64) {
        let mut start: Option<(Arc<SshConnection>, i64)> = None;
        {
            let mut sessions = self.sessions.lock().unwrap();
            let Some(entry) = sessions.get_mut(session_id) else { return };
            if entry.kind != "metrics" {
                return;
            }
            if let Some(task) = entry.metrics_task.take() {
                task.abort();
            }
            if interval_sec <= 0 {
                return;
            }
            match &entry.conn {
                Some(conn) => start = Some((conn.clone(), interval_sec)),
                None => entry.metrics_pending = Some(interval_sec),
            }
        }
        if let Some((conn, sec)) = start {
            self.start_metrics(session_id, sec, &conn);
        }
    }

    fn start_metrics(&self, session_id: &str, interval_sec: i64, conn: &Arc<SshConnection>) {
        let this = self.clone();
        let task_session_id = session_id.to_string();
        let conn = conn.clone();
        let task = tokio::spawn(async move {
            let session_id = task_session_id;
            let mut first = true;
            loop {
                if !first {
                    tokio::time::sleep(std::time::Duration::from_secs(interval_sec.max(1) as u64)).await;
                }
                first = false;
                // 会话已关闭则停止
                if !this.sessions.lock().unwrap().contains_key(&session_id) {
                    return;
                }
                match conn.exec(&metrics::build_metrics_command(), 10_000, 2 * 1024 * 1024).await {
                    Ok(r) if !r.timed_out => {
                        this.send_control(ServerFrame::MetricsData {
                            s: session_id.clone(),
                            m: metrics::parse_metrics(&r.stdout),
                        });
                    }
                    Ok(_) => {
                        this.send_control(ServerFrame::error(
                            Some(&session_id),
                            "metrics-failed",
                            "远程命令执行超时",
                        ));
                    }
                    Err(e) => {
                        this.send_control(ServerFrame::error(
                            Some(&session_id),
                            "metrics-failed",
                            e,
                        ));
                    }
                }
            }
        });
        if let Some(entry) = self.sessions.lock().unwrap().get_mut(session_id) {
            entry.metrics_task = Some(task);
        }
    }

    // ── 生命周期 ───────────────────────────────────────────────────────────

    pub fn resolve_host_key(&self, session_id: &str, accepted: bool) {
        let slot = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).map(|e| e.confirm_slot.clone())
        };
        if let Some(slot) = slot {
            if let Some(tx) = slot.lock().unwrap().take() {
                let _ = tx.send(accepted);
            }
        }
    }

    /// 密码重试应答：Some(密码) = 用新密码重试；None = 取消（会话随即关闭）
    pub fn resolve_auth(&self, session_id: &str, password: Option<String>) {
        let slot = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).map(|e| e.auth_slot.clone())
        };
        if let Some(slot) = slot {
            if let Some(tx) = slot.lock().unwrap().take() {
                let _ = tx.send(password);
            }
        }
    }

    pub fn close(&self, session_id: &str, reason: &str) {
        // 先回应挂起的 TOFU 询问（拒绝）与密码重试（取消），再走统一清理
        let slots = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(session_id)
                .map(|e| (e.confirm_slot.clone(), e.auth_slot.clone()))
        };
        if let Some((confirm_slot, auth_slot)) = slots {
            if let Some(tx) = confirm_slot.lock().unwrap().take() {
                let _ = tx.send(false);
            }
            if let Some(tx) = auth_slot.lock().unwrap().take() {
                let _ = tx.send(None);
            }
        }
        self.handle_closed(session_id, reason);
    }

    pub fn close_all(&self, reason: &str) {
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.close(&id, reason);
        }
    }

    fn handle_closed(&self, session_id: &str, reason: &str) {
        let removed = self.sessions.lock().unwrap().remove(session_id);
        let Some(mut entry) = removed else { return };
        if let Some(tx) = entry.confirm_slot.lock().unwrap().take() {
            let _ = tx.send(false);
        }
        if let Some(tx) = entry.auth_slot.lock().unwrap().take() {
            let _ = tx.send(None);
        }
        if let Some(task) = entry.metrics_task.take() {
            task.abort();
        }
        // 传输/上传任务收尾；底层 SFTP 通道由注册表统一管理（多会话共享，不在此关闭）
        if let Some(sftp) = entry.sftp.take() {
            tokio::spawn(async move {
                sftp.close_ops().await;
            });
        }
        // 释放连接持有者：同主机无其他持有者时进入空闲回收计时
        if let Some(host_id) = entry.host_id.take() {
            self.registry.release(&host_id, session_id);
        }
        // stdin 写入任务随 sender drop 结束；PTY 通道随 Arc drop 关闭（远端 shell 收 EOF）
        entry.stdin_tx.take();
        entry.pty.take();
        self.send_control(ServerFrame::SessionClosed {
            s: session_id.to_string(),
            m: protocol::SessionClosedM {
                reason: reason.to_string(),
            },
        });
        self.stores.audit.append(
            "session.closed",
            serde_json::json!({ "sessionId": session_id, "reason": reason }),
        );
    }

    // ── 主机 CRUD / 探测（控制帧分发辅助）──────────────────────────────────

    pub fn handle_hosts_op(&self, m: &serde_json::Value) {
        let op = m.get("op").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        match op.as_str() {
            "list" => {
                let hosts = self.stores.hosts.list();
                self.send_control(ServerFrame::Hosts {
                    m: HostsResultM {
                        op,
                        ok: true,
                        hosts: Some(hosts),
                        host: None,
                        message: None,
                    },
                });
            }
            "create" | "update" => {
                let input = serde_json::from_value::<bg_core::protocol::HostInput>(
                    m.get("host").cloned().unwrap_or(serde_json::Value::Null),
                );
                match input {
                    Ok(input) => {
                        let result = if op == "create" {
                            self.stores.hosts.create(&input).map(Some)
                        } else {
                            let id = m.get("host").and_then(|h| h.get("id")).and_then(|v| v.as_str()).unwrap_or_default();
                            self.stores.hosts.update(id, &input)
                        };
                        match result {
                            Ok(Some(host)) => {
                                // 主机档案变更（改密/改址/改端口）：驱逐池内旧连接，
                                // 避免改密后残留旧凭据的连接继续可用
                                if op == "update" {
                                    self.registry.evict(&host.id, "host-updated");
                                }
                                self.send_control(ServerFrame::Hosts {
                                    m: HostsResultM { op, ok: true, hosts: None, host: Some(host), message: None },
                                })
                            }
                            Ok(None) => self.send_control(ServerFrame::Hosts {
                                m: HostsResultM { op, ok: false, hosts: None, host: None, message: Some("主机档案不存在".into()) },
                            }),
                            Err(e) => self.send_control(ServerFrame::Hosts {
                                m: HostsResultM { op, ok: false, hosts: None, host: None, message: Some(e.to_string()) },
                            }),
                        }
                    }
                    Err(e) => self.send_control(ServerFrame::Hosts {
                        m: HostsResultM { op, ok: false, hosts: None, host: None, message: Some(format!("主机档案格式错误: {e}")) },
                    }),
                }
            }
            "delete" => {
                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                let ok = self.stores.hosts.delete(id);
                if ok {
                    // 档案删除：池内连接立即驱逐（持有方会话由存活检测自行感知关闭）
                    self.registry.evict(id, "host-deleted");
                }
                self.send_control(ServerFrame::Hosts {
                    m: HostsResultM {
                        op,
                        ok,
                        hosts: None,
                        host: None,
                        message: (!ok).then(|| "主机档案不存在".to_string()),
                    },
                });
            }
            _ => {}
        }
    }

    pub async fn probe(&self, host_id: &str) {
        let frame = match self.stores.hosts.get(host_id) {
            None => ProbeResultM {
                host_id: host_id.to_string(),
                ok: false,
                ms: None,
                message: Some("主机档案不存在".into()),
            },
            // probe_tcp 不感知 hostId（其调用方 toolcore 也无需），此处回填以供前端按 hostId 匹配
            Some(host) => {
                let mut r = probe_tcp(&host.input.host, host.input.port).await;
                r.host_id = host_id.to_string();
                r
            }
        };
        self.send_control(ServerFrame::ProbeResult { m: frame });
    }

    /// 测试连接（添加/编辑主机表单）：用表单凭据做一次真实 SSH 认证。
    /// 未知主机指纹自动接受（不写 known_hosts，正式连接仍走 TOFU 确认）；
    /// 已知指纹不匹配立即拒绝；认证只试一次，被拒即失败；不落主机档案、不入连接池。
    pub async fn test_host(&self, input: protocol::HostInput) {
        let host = protocol::HostRecord {
            input,
            id: "test".into(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let target = format!(
            "{}@{}:{}",
            host.input.username, host.input.host, host.input.port
        );
        let mismatch = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = Arc::new(crate::ssh::HostKeyCtx {
            known: self.stores.known.clone(),
            host: host.input.host.clone(),
            port: host.input.port,
            ask: HostKeyAsk::AutoAccept,
            mismatch: mismatch.clone(),
            save_password: None,
        });
        let (ok, message) = match crate::ssh::connect(&host, ctx).await {
            Ok(conn) => {
                conn.close().await;
                (true, "认证成功，凭据有效".to_string())
            }
            Err(e) => {
                let msg = if mismatch.load(std::sync::atomic::Ordering::Relaxed) {
                    format!(
                        "主机 {}:{} 指纹不匹配（疑似中间人攻击），已拒绝连接",
                        host.input.host, host.input.port
                    )
                } else {
                    e.message()
                };
                (false, msg)
            }
        };
        self.stores.audit.append(
            "host.test",
            serde_json::json!({ "target": target, "ok": ok, "message": message }),
        );
        self.send_control(ServerFrame::TestHostResult {
            m: protocol::TestHostResultM {
                ok,
                message: Some(message),
            },
        });
    }
}

fn ssh_kind_matches(kind: &str, want: &str) -> bool {
    // kind 缺省 = shell
    let k = if kind.is_empty() { "shell" } else { kind };
    k == want
}

fn ok_sftp(op: &str, rid: Option<String>, fill: impl FnOnce(&mut SftpResultM)) -> SftpResultM {
    let mut m = SftpResultM {
        op: op.to_string(),
        ok: true,
        rid,
        op_id: None,
        resolved_path: None,
        entries: None,
        size_b: None,
        host: None,
        message: None,
        error_kind: None,
        preflight: None,
        dl_base: None,
    };
    fill(&mut m);
    m
}

fn err_sftp(op: &str, rid: Option<String>, message: &str) -> SftpResultM {
    SftpResultM {
        op: op.to_string(),
        ok: false,
        rid,
        op_id: None,
        resolved_path: None,
        entries: None,
        size_b: None,
        host: None,
        message: Some(message.to_string()),
        // 统一注入错误分类（权限/空间/重名/占用/网络），前端据此给友好提示与建议
        error_kind: crate::sftp::classify_sftp_error(message),
        preflight: None,
        dl_base: None,
    }
}

/// 组装传输预检报告（preflight op 与 AI 工具 sftp_*_file 共用）
/// 方向语义：upload → path=本地源（Agent 本地盘，浏览器宿主用 size 提示）、to=远端目标；
///          download → path=远端源、to=本地目标。
pub async fn build_preflight_report(
    ctrl: &SftpController,
    direction: &str,
    path: &str,
    to: &str,
    size_hint: Option<u64>,
) -> Result<bg_core::protocol::PreflightReportM, String> {
    use bg_core::protocol::{PreflightItemM, PreflightReportM};

    const LARGE_FILE: u64 = 1024 * 1024 * 1024; // ≥1GB 提醒（交互阈值，双端一致）
    fn item(code: &str, message: String) -> PreflightItemM {
        PreflightItemM {
            code: code.to_string(),
            message,
        }
    }
    fn fmt_b(n: u64) -> String {
        const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let mut v = n as f64;
        let mut i = 0usize;
        while v >= 1024.0 && i < U.len() - 1 {
            v /= 1024.0;
            i += 1;
        }
        if i == 0 {
            format!("{n} B")
        } else {
            format!("{v:.1} {}", U[i])
        }
    }

    let mut report = PreflightReportM {
        size_b: 0,
        file_count: 1,
        scan_truncated: false,
        remote_exists: false,
        remote_size_b: None,
        remote_free_b: None,
        remote_writable: true,
        local_exists: false,
        local_free_b: None,
        local_fs: None,
        local_max_file_b: None,
        suggested_path: None,
        warnings: Vec::new(),
        blockers: Vec::new(),
    };
    let budget = crate::sftp::ScanBudget::new();

    if direction == "download" {
        // 远端源
        let rc = ctrl.remote_preflight(path, false).await;
        if !rc.exists {
            report.blockers.push(item("not_found", "远端文件/目录不存在".into()));
            return Ok(report);
        }
        let (size, count, truncated) = if rc.is_dir {
            ctrl.scan_remote_size(path, &budget).await?
        } else {
            (rc.size_b.unwrap_or(0), 1, false)
        };
        report.size_b = size;
        report.file_count = count;
        report.scan_truncated = truncated;
        if truncated {
            report
                .warnings
                .push(item("scan_truncated", "目录过大，扫描被截断，大小为下限估计".into()));
        }
        // 本地目标
        let local = std::path::Path::new(to);
        report.local_exists = local.exists();
        if let Some(free) = bg_core::disk::available_space(local) {
            report.local_free_b = Some(free);
            if size > free {
                report.blockers.push(item(
                    "no_space",
                    format!("本地磁盘剩余空间不足：需 {}，仅剩 {}", fmt_b(size), fmt_b(free)),
                ));
            }
        }
        if let Some(fs) = bg_core::disk::fs_name(local) {
            report.local_fs = Some(fs.clone());
            if let Some(max) = bg_core::disk::max_file_bytes(&fs) {
                report.local_max_file_b = Some(max);
                // FAT32 单文件上限仅对单文件下载可判定（目录任务逐文件各异）
                if count <= 1 && size > max {
                    report.blockers.push(item(
                        "fs_4gb_limit",
                        format!("目标盘为 {}，单文件上限 {}，文件 {} 无法保存（换 NTFS/exFAT 目标）", fs, fmt_b(max), fmt_b(size)),
                    ));
                }
            }
        }
    } else {
        // 上传：本地源（Agent 本地盘）
        let local = std::path::Path::new(path);
        if local.exists() {
            let md = tokio::fs::metadata(local)
                .await
                .map_err(|e| format!("读取本地路径失败：{e}"))?;
            if md.is_dir() {
                let (t, c, tr) = crate::sftp::scan_local_size(local, &budget)?;
                report.size_b = t;
                report.file_count = c;
                report.scan_truncated = tr;
                if tr {
                    report
                        .warnings
                        .push(item("scan_truncated", "目录过大，扫描被截断，大小为下限估计".into()));
                }
            } else {
                report.size_b = md.len();
            }
        } else {
            // 浏览器宿主无本地路径：用调用方给出的总量提示
            report.size_b = size_hint.unwrap_or(0);
        }
        // 远端目标
        let rc = ctrl.remote_preflight(to, true).await;
        report.remote_exists = rc.exists;
        report.remote_size_b = if rc.exists { rc.size_b } else { None };
        report.remote_free_b = rc.free_b;
        report.remote_writable = rc.writable;
        report.suggested_path = rc.suggested_path;
        if rc.exists && rc.is_dir {
            report
                .blockers
                .push(item("target_exists", "远端目标路径已是一个目录，无法覆盖".into()));
        } else if rc.exists {
            report.remote_size_b = rc.size_b;
        }
        if !rc.writable {
            report
                .blockers
                .push(item("no_permission", "远端目标目录不可写（无权限或只读）".into()));
        }
        if let Some(free) = rc.free_b {
            if report.size_b > free {
                report.blockers.push(item(
                    "no_space",
                    format!("远端剩余空间不足：需 {}，仅剩 {}", fmt_b(report.size_b), fmt_b(free)),
                ));
            }
        } else {
            report.warnings.push(item(
                "free_unknown",
                "服务器不支持剩余空间查询，无法预检远端磁盘空间".into(),
            ));
        }
    }

    if report.blockers.is_empty() && report.size_b >= LARGE_FILE {
        report.warnings.push(item(
            "large_file",
            format!("传输量较大（{}），耗时可能较长", fmt_b(report.size_b)),
        ));
    }
    Ok(report)
}

/// 原生传输的 path/to 双路径校验
fn require_paths<'a>(
    path: Option<&'a str>,
    to: Option<&'a str>,
) -> Result<(&'a str, &'a str), &'a str> {
    match (path.filter(|p| !p.is_empty()), to.filter(|t| !t.is_empty())) {
        (Some(p), Some(t)) => Ok((p, t)),
        _ => Err("缺少 path/to 路径"),
    }
}

/// TCP 连通性探测（默认 2.5s 超时；不做 SSH 握手，避免凭据参与）
pub async fn probe_tcp(host: &str, port: u16) -> ProbeResultM {
    let start = std::time::Instant::now();
    let connect = tokio::time::timeout(
        std::time::Duration::from_millis(2500),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await;
    match connect {
        Ok(Ok(_)) => ProbeResultM {
            host_id: String::new(),
            ok: true,
            ms: Some(start.elapsed().as_millis() as u64),
            message: None,
        },
        Ok(Err(e)) => ProbeResultM {
            host_id: String::new(),
            ok: false,
            ms: None,
            message: Some(e.to_string()),
        },
        Err(_) => ProbeResultM {
            host_id: String::new(),
            ok: false,
            ms: None,
            message: Some("连接超时".into()),
        },
    }
}
