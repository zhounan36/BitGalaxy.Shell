//! 会话管理器：一个 WS 连接对应一个实例。
//! 三种会话 kind 共享 SSH 建连/TOFU/审计路径；sftp/metrics 通道按 kind 挂载。
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::task::JoinHandle;

use crate::metrics;
use crate::protocol::{
    self, HostsResultM, ProbeResultM, ServerFrame, SessionOpenedM, SftpResultM, DF_STDOUT,
};
use crate::sftp::SftpController;
use crate::ssh::{self, ConfirmSlot, HostKeyAsk, HostKeyCtx, SshConnection, SshKind};
use crate::stores::Stores;
use crate::wire::Outbound;

type SessionMap = Arc<StdMutex<HashMap<String, SessionEntry>>>;

struct SessionEntry {
    kind: String,
    conn: Option<Arc<SshConnection>>,
    sftp: Option<Arc<SftpController>>,
    metrics_task: Option<JoinHandle<()>>,
    /// 建连期间到达的 metrics 订阅，就绪后自动启动
    metrics_pending: Option<i64>,
    confirm_slot: ConfirmSlot,
}

impl SessionEntry {
    fn new(kind: &str) -> SessionEntry {
        SessionEntry {
            kind: kind.to_string(),
            conn: None,
            sftp: None,
            metrics_task: None,
            metrics_pending: None,
            confirm_slot: Arc::new(StdMutex::new(None)),
        }
    }
}

#[derive(Clone)]
pub struct SessionManager {
    stores: Stores,
    out: Outbound,
    sessions: SessionMap,
}

impl SessionManager {
    pub fn new(stores: Stores, out: Outbound) -> SessionManager {
        SessionManager {
            stores,
            out,
            sessions: Arc::new(StdMutex::new(HashMap::new())),
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

            let confirm_slot = {
                let sessions = this.sessions.lock().unwrap();
                sessions.get(&session_id).map(|e| e.confirm_slot.clone())
            };
            let Some(confirm_slot) = confirm_slot else { return };

            let mismatch = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let ctx = Arc::new(HostKeyCtx {
                known: this.stores.known.clone(),
                host: host.input.host.clone(),
                port: host.input.port,
                ask: HostKeyAsk::Interactive {
                    session_id: session_id.clone(),
                    out: this.out.clone(),
                    slot: confirm_slot,
                },
                mismatch: mismatch.clone(),
            });

            let ssh_kind = match kind.as_str() {
                "sftp" => SshKind::Sftp,
                "metrics" => SshKind::Metrics,
                _ => SshKind::Shell,
            };
            let conn = match ssh::connect(&host, &ssh_kind, ctx).await {
                Ok(c) => c,
                Err(e) => {
                    let reason = if mismatch.load(std::sync::atomic::Ordering::Relaxed) {
                        "主机指纹校验未通过（疑似中间人攻击）".to_string()
                    } else {
                        e.message()
                    };
                    this.fail_open(&session_id, &reason);
                    return;
                }
            };
            let conn = Arc::new(conn);

            // shell：接管读半部，stdout/stderr → 0x01 数据帧；流结束 → 会话关闭
            if ssh_kind_matches(&kind, "shell") {
                let reader = conn.shell_reader.lock().unwrap().take();
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
                // 记录初始 PTY 尺寸
                let _ = conn.resize(cols, rows).await;
            }

            // sftp：打开子系统控制器
            let sftp = if ssh_kind_matches(&kind, "sftp") {
                match conn.open_sftp().await {
                    Ok(s) => Some(Arc::new(SftpController::new(s))),
                    Err(e) => {
                        this.fail_open(&session_id, &e);
                        return;
                    }
                }
            } else {
                None
            };

            // 非 shell 会话：连接断开监视（server 主动断开 → session-closed）
            if !ssh_kind_matches(&kind, "shell") {
                let this2 = this.clone();
                let sid = session_id.clone();
                let conn2 = conn.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        if !conn2.is_alive() {
                            this2.handle_closed(&sid, "client-closed");
                            return;
                        }
                    }
                });
            }

            let pending_metrics = {
                let mut sessions = this.sessions.lock().unwrap();
                match sessions.get_mut(&session_id) {
                    None => return,
                    Some(entry) => {
                        entry.conn = Some(conn.clone());
                        entry.sftp = sftp.clone();
                        entry.metrics_pending.take()
                    }
                }
            };

            this.send_control(ServerFrame::SessionOpened {
                s: session_id.clone(),
                m: SessionOpenedM {
                    host_id: host_id.clone(),
                    kind: kind.clone(),
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

    pub async fn input(&self, session_id: &str, data: &[u8]) {
        let conn = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).and_then(|e| e.conn.clone())
        };
        if let Some(conn) = conn {
            let _ = conn.write_input(data).await;
        }
    }

    pub async fn resize(&self, session_id: &str, cols: u32, rows: u32) {
        let conn = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).and_then(|e| e.conn.clone())
        };
        if let Some(conn) = conn {
            let _ = conn.resize(cols, rows).await;
        }
    }

    // ── sftp ───────────────────────────────────────────────────────────────

    pub fn handle_sftp(&self, session_id: &str, op: &protocol::SftpOpM) {
        let reply = |m: SftpResultM| {
            self.send_control(ServerFrame::SftpResult {
                s: session_id.to_string(),
                m,
            })
        };
        let sftp = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).and_then(|e| e.sftp.clone())
        };
        let Some(sftp) = sftp else {
            reply(err_sftp(&op.op, Some(op.rid.clone()), "SFTP 会话未就绪"));
            return;
        };

        let op_clone = op.clone();
        let session_id = session_id.to_string();
        let out = self.out.clone();
        tokio::spawn(async move {
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
                    let op_id = sftp.upload_open(op_clone.path.as_deref().unwrap_or_default());
                    ok_sftp("upload-open", Some(op_clone.rid.clone()), |m| {
                        m.op_id = Some(op_id);
                    })
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
                other => err_sftp(other, Some(op_clone.rid.clone()), "未知 SFTP 操作"),
            };
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

    pub fn close(&self, session_id: &str, reason: &str) {
        // 先回应挂起的 TOFU 询问（拒绝），再走统一清理
        let slot = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).map(|e| e.confirm_slot.clone())
        };
        if let Some(slot) = slot {
            if let Some(tx) = slot.lock().unwrap().take() {
                let _ = tx.send(false);
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
        if let Some(task) = entry.metrics_task.take() {
            task.abort();
        }
        if let Some(sftp) = entry.sftp.take() {
            tokio::spawn(async move {
                sftp.close().await;
            });
        }
        if let Some(conn) = entry.conn.take() {
            tokio::spawn(async move {
                conn.close().await;
            });
        }
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
                let input = serde_json::from_value::<crate::protocol::HostInput>(
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
                            Ok(Some(host)) => self.send_control(ServerFrame::Hosts {
                                m: HostsResultM { op, ok: true, hosts: None, host: Some(host), message: None },
                            }),
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
            Some(host) => probe_tcp(&host.input.host, host.input.port).await,
        };
        self.send_control(ServerFrame::ProbeResult { m: frame });
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
        message: None,
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
        message: Some(message.to_string()),
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
