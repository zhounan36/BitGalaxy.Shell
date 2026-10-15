//! 统一 SSH 连接注册表（ConnRegistry）：工作台会话 / AI 工具 / MCP 三端共用。
//! 同主机共享一条 TCP 连接（shell PTY / SFTP / exec / metrics 全部多通道复用），
//! 生命周期按持有者计数管理：holders 非空绝不回收；全部释放后按空闲超时关闭。
//! MCP stdio 为独立进程，自建实例（代码同源）。
//! 底层 conn/sftp 的关闭唯一归口本模块（idle 回收 / evict / close_all）。
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use russh_sftp::client::SftpSession;

use crate::ssh::{self, HostKeyAsk, HostKeyCtx, SshConnection, SshError};
use bg_core::error::{
    ToolError, ERR_AUTH_FAILED, ERR_CONNECT_FAILED, ERR_HOST_NOT_FOUND, ERR_HOSTKEY_MISMATCH,
    ERR_HOSTKEY_UNKNOWN, ERR_SFTP_FAILED,
};
use bg_core::stores::Stores;

struct RegistryEntry {
    conn: Arc<SshConnection>,
    sftp: Option<Arc<SftpSession>>,
    /// 持有者（工作台 sessionId / AI 临时占用 id）；非空时连接不参与空闲回收
    holders: HashSet<String>,
    idle_task: tokio::task::JoinHandle<()>,
}

/// 建连上下文：TOFU 策略按发起方（工作台 Interactive / AI·MCP FailClosed）
pub struct CreateCtx {
    pub ask: HostKeyAsk,
    /// 审计标识：workbench / pet / MCP clientInfo
    pub initiator: String,
}

/// AI 临时占用守卫：Drop 自动释放持有者（连接在无其他持有者时按空闲超时回收）
pub struct TransientLease {
    registry: ConnRegistry,
    host_id: String,
    holder: String,
    pub conn: Arc<SshConnection>,
}

impl Drop for TransientLease {
    fn drop(&mut self) {
        self.registry.release(&self.host_id, &self.holder);
    }
}

/// SFTP 临时占用：lease 守卫 + 池缓存的 SFTP 通道（守卫存活期间连接不会被空闲回收）
pub struct SftpLease {
    lease: TransientLease,
    pub sftp: Arc<SftpSession>,
}

impl SftpLease {
    pub fn conn(&self) -> &Arc<SshConnection> {
        &self.lease.conn
    }
}

#[derive(Clone)]
pub struct ConnRegistry {
    stores: Stores,
    idle_close_sec: u64,
    entries: Arc<StdMutex<HashMap<String, RegistryEntry>>>,
    locks: Arc<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl ConnRegistry {
    pub fn new(stores: Stores, idle_close_sec: u64) -> ConnRegistry {
        ConnRegistry {
            stores,
            idle_close_sec,
            entries: Arc::new(StdMutex::new(HashMap::new())),
            locks: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    fn host_lock(&self, host_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap();
        locks
            .entry(host_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 持久持有（工作台会话）：holders 计数 +1；会话关闭时 [`ConnRegistry::release`]。
    /// TOFU 由首个建连方的 CreateCtx.ask 决定（后续复用不再触发）。
    pub async fn acquire_session(
        &self,
        host_id: &str,
        holder_id: &str,
        create: CreateCtx,
    ) -> Result<Arc<SshConnection>, ToolError> {
        if let Some(conn) = self.acquire_existing(host_id, Some(holder_id)) {
            return Ok(conn);
        }
        let lock = self.host_lock(host_id);
        let _g = lock.lock().await;
        if let Some(conn) = self.acquire_existing(host_id, Some(holder_id)) {
            return Ok(conn);
        }
        let conn = self.connect_new(host_id, create).await?;
        self.insert_entry(host_id, conn.clone(), Some(holder_id));
        Ok(conn)
    }

    /// AI/MCP 临时占用：Drop 自动释放。TOFU fail-closed（未知指纹直接拒绝，
    /// 保留「先在工作台人工确认一次」的语义）。
    pub async fn acquire_transient(
        &self,
        host_id: &str,
        initiator: &str,
    ) -> Result<TransientLease, ToolError> {
        let holder = format!("t-{}", uuid::Uuid::new_v4());
        let conn = self
            .acquire_session(
                host_id,
                &holder,
                CreateCtx {
                    ask: HostKeyAsk::FailClosed,
                    initiator: initiator.to_string(),
                },
            )
            .await?;
        Ok(TransientLease {
            registry: self.clone(),
            host_id: host_id.to_string(),
            holder,
            conn,
        })
    }

    /// AI/MCP 临时占用 SFTP 通道：复用池内缓存的 SftpSession（同一连接同一通道），
    /// 守卫存活期间连接不会被空闲回收（长传输不再被 300s 空闲回收打断）。
    pub async fn acquire_sftp(&self, host_id: &str, initiator: &str) -> Result<SftpLease, ToolError> {
        let lease = self.acquire_transient(host_id, initiator).await?;
        let sftp = self.sftp_channel(host_id).await?;
        Ok(SftpLease { lease, sftp })
    }

    /// 持久持有者取 SFTP 通道（工作台 sftp 会话 / shell 拖拽惰性上传）：
    /// 调用方需已 acquire_session 持有该主机；通道池内缓存共享。
    pub async fn shared_sftp(&self, host_id: &str) -> Result<Arc<SftpSession>, ToolError> {
        self.sftp_channel(host_id).await
    }

    /// 释放持有者；全部释放后重启空闲回收计时
    pub fn release(&self, host_id: &str, holder_id: &str) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(host_id) {
            entry.holders.remove(holder_id);
            if entry.holders.is_empty() {
                drop(entries);
                self.touch_idle(host_id);
            }
        }
    }

    /// 立即驱逐（主机档案删除/修改、连接损坏）：关闭 conn+sftp，唯一归口。
    /// 持有者侧由各自的存活检测（shell reader / 看门狗 / 操作报错）感知关闭。
    pub fn evict(&self, host_id: &str, reason: &str) {
        let removed = self.entries.lock().unwrap().remove(host_id);
        if let Some(entry) = removed {
            entry.idle_task.abort();
            let target = entry.conn.host_label.clone();
            self.close_entry(entry);
            self.stores.audit.append(
                "connection.closed",
                serde_json::json!({ "hostId": host_id, "target": target, "reason": reason }),
            );
        }
    }

    pub fn close_all(&self, reason: &str) {
        let ids: Vec<String> = self.entries.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.evict(&id, reason);
        }
    }

    // ── 内部实现 ───────────────────────────────────────────────────────────

    /// 快路径：活连接 → 计持有者并续空闲计时；死条目 → 摘除并异步关闭（走重建）
    fn acquire_existing(&self, host_id: &str, holder_id: Option<&str>) -> Option<Arc<SshConnection>> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get_mut(host_id)?;
        if !entry.conn.is_alive() {
            let dead = entries.remove(host_id);
            drop(entries);
            if let Some(e) = dead {
                e.idle_task.abort();
                self.close_entry(e);
            }
            return None;
        }
        if let Some(h) = holder_id {
            entry.holders.insert(h.to_string());
        }
        let conn = entry.conn.clone();
        drop(entries);
        self.touch_idle(host_id);
        Some(conn)
    }

    /// 建连（须持 host_lock）：错误映射与审计在此归口
    async fn connect_new(&self, host_id: &str, create: CreateCtx) -> Result<Arc<SshConnection>, ToolError> {
        let host = self
            .stores
            .hosts
            .get(host_id)
            .ok_or_else(|| ToolError::new(ERR_HOST_NOT_FOUND, format!("主机档案不存在: {host_id}")))?;
        let mismatch = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // 密码重试认证成功 → 回写主机档案（下次连接免重输）+ 审计 + 推送最新主机列表；
        // 仅工作台交互模式（AI/MCP fail-closed 无重试，不回写）
        let save_password: Option<Arc<dyn Fn(&str) + Send + Sync>> = match &create.ask {
            HostKeyAsk::Interactive { out, .. } => {
                let out = out.clone();
                let stores = self.stores.clone();
                let hid = host_id.to_string();
                Some(Arc::new(move |pwd: &str| {
                    if !stores.hosts.update_password(&hid, pwd) {
                        return;
                    }
                    stores.audit.append(
                        "host.password_updated",
                        serde_json::json!({ "hostId": hid, "reason": "auth-retry" }),
                    );
                    out.send_frame(&bg_core::protocol::ServerFrame::Hosts {
                        m: bg_core::protocol::HostsResultM {
                            op: "list".into(),
                            ok: true,
                            hosts: Some(stores.hosts.list()),
                            host: None,
                            message: None,
                        },
                    });
                }))
            }
            HostKeyAsk::FailClosed | HostKeyAsk::AutoAccept => None,
        };
        let ctx = Arc::new(HostKeyCtx {
            known: self.stores.known.clone(),
            host: host.input.host.clone(),
            port: host.input.port,
            ask: create.ask,
            mismatch: mismatch.clone(),
            save_password,
        });
        match ssh::connect(&host, ctx).await {
            Ok(conn) => {
                let conn = Arc::new(conn);
                self.stores.audit.append(
                    "connection.opened",
                    serde_json::json!({
                        "hostId": host_id,
                        "target": conn.host_label,
                        "initiator": create.initiator,
                    }),
                );
                Ok(conn)
            }
            Err(e) => {
                if mismatch.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(ToolError::new(
                        ERR_HOSTKEY_MISMATCH,
                        format!(
                            "主机 {}:{} 指纹不匹配（疑似中间人攻击），已拒绝连接",
                            host.input.host, host.input.port
                        ),
                    ));
                }
                match e {
                    SshError::HostKeyRejected => Err(ToolError::new(
                        ERR_HOSTKEY_UNKNOWN,
                        format!(
                            "主机 {}（{}:{}）指纹未确认：请先在 BitGalaxy Shell 工作台人工连接一次（TOFU 确认）后再交给 AI 使用",
                            host.input.name, host.input.host, host.input.port
                        ),
                    )),
                    SshError::Auth(msg) => Err(ToolError::new(
                        ERR_AUTH_FAILED,
                        format!("认证失败（{}@{}）: {msg}", host.input.username, host.input.host),
                    )),
                    SshError::Timeout | SshError::Connect { .. } => {
                        Err(ToolError::new(ERR_CONNECT_FAILED, e.message()))
                    }
                    SshError::Other(msg) => Err(ToolError::new(ERR_CONNECT_FAILED, msg)),
                }
            }
        }
    }

    /// 取（或打开并缓存）host 的 SFTP 通道；要求条目已存在（调用方先持有连接）。
    /// host_lock 串行化并发打开去重。
    async fn sftp_channel(&self, host_id: &str) -> Result<Arc<SftpSession>, ToolError> {
        {
            let mut entries = self.entries.lock().unwrap();
            if let Some(entry) = entries.get_mut(host_id) {
                if !entry.conn.is_alive() {
                    return Err(ToolError::new(ERR_CONNECT_FAILED, "连接已关闭"));
                }
                if let Some(sftp) = entry.sftp.clone() {
                    drop(entries);
                    self.touch_idle(host_id);
                    return Ok(sftp);
                }
            } else {
                return Err(ToolError::new(ERR_CONNECT_FAILED, "连接未建立"));
            }
        }
        let lock = self.host_lock(host_id);
        let _g = lock.lock().await;
        {
            let mut entries = self.entries.lock().unwrap();
            if let Some(entry) = entries.get_mut(host_id) {
                if let Some(sftp) = entry.sftp.clone() {
                    drop(entries);
                    return Ok(sftp);
                }
            }
        }
        let conn = self
            .entries
            .lock()
            .unwrap()
            .get(host_id)
            .map(|e| e.conn.clone())
            .ok_or_else(|| ToolError::new(ERR_CONNECT_FAILED, "连接未建立"))?;
        let sftp = conn
            .open_sftp()
            .await
            .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("SFTP 通道打开失败: {e}")))?;
        let sftp = Arc::new(sftp);
        if let Some(entry) = self.entries.lock().unwrap().get_mut(host_id) {
            entry.sftp = Some(sftp.clone());
        }
        Ok(sftp)
    }

    fn insert_entry(&self, host_id: &str, conn: Arc<SshConnection>, holder: Option<&str>) {
        let mut holders = HashSet::new();
        if let Some(h) = holder {
            holders.insert(h.to_string());
        }
        self.entries
            .lock()
            .unwrap()
            .insert(host_id.to_string(), RegistryEntry {
                conn,
                sftp: None,
                holders,
                idle_task: tokio::spawn(async {}),
            });
        self.touch_idle(host_id);
    }

    /// 重置空闲计时：到期时 holders 为空才回收，否则顺延一轮
    fn touch_idle(&self, host_id: &str) {
        let registry = self.clone();
        let id = host_id.to_string();
        let idle_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(registry.idle_close_sec.max(1))).await;
            registry.idle_fire(&id);
        });
        if let Some(entry) = self.entries.lock().unwrap().get_mut(host_id) {
            entry.idle_task.abort();
            entry.idle_task = idle_task;
        } else {
            idle_task.abort();
        }
    }

    fn idle_fire(&self, host_id: &str) {
        let mut entries = self.entries.lock().unwrap();
        match entries.get(host_id) {
            Some(entry) if entry.holders.is_empty() => {
                let removed = entries.remove(host_id);
                drop(entries);
                if let Some(entry) = removed {
                    entry.idle_task.abort();
                    let target = entry.conn.host_label.clone();
                    self.close_entry(entry);
                    self.stores.audit.append(
                        "connection.closed",
                        serde_json::json!({
                            "hostId": host_id,
                            "target": target,
                            "reason": "idle",
                        }),
                    );
                }
            }
            // 仍有持有者：顺延下一轮
            Some(_) => {
                drop(entries);
                self.touch_idle(host_id);
            }
            None => {}
        }
    }

    /// 异步关闭 conn+sftp（先 sftp 后 conn；不阻塞调用方）
    fn close_entry(&self, entry: RegistryEntry) {
        let conn = entry.conn;
        let sftp = entry.sftp;
        tokio::spawn(async move {
            if let Some(s) = sftp {
                let _ = s.close().await;
            }
            conn.close().await;
        });
    }
}
