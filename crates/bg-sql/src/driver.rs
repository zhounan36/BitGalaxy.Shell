//! 驱动宿主进程管理 + stdio NDJSON-RPC 客户端。
//! 每种数据库驱动是独立可执行程序（内置 mysql_async / tokio-postgres / rusqlite），
//! Agent 主程序不含数据库协议实现；崩溃隔离——驱动挂掉只影响该驱动的请求。
//!
//! 协议（每行一条 JSON）：
//!   请求：{"id":u64,"method":"...","params":{...}}
//!   响应：{"id":u64,"result":{...}} 或 {"id":u64,"error":{"code","kind","message","retryable"}}
//!   通知：{"method":"log"|"progress","params":{...}}（无 id）
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex};

const INIT_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("driver_crashed: 驱动进程已退出")]
    Crashed,
    #[error("driver_unavailable: 无法启动驱动进程: {0}")]
    Unavailable(String),
    #[error("timeout: 驱动响应超时（{method}）")]
    Timeout { method: String },
    #[error("{kind}: {message}")]
    Remote { code: String, kind: String, message: String, retryable: bool },
}

struct Pending {
    tx: oneshot::Sender<Result<serde_json::Value, DriverError>>,
}

/// 驱动宿主进程的 RPC 通道（按驱动惰性 spawn，复用到空闲回收）
pub struct DriverHost {
    pub name: String,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    next_id: AtomicU64,
    /// 进程是否已退出（fail-fast 标记）
    dead: Arc<std::sync::atomic::AtomicBool>,
}

impl DriverHost {
    /// 启动驱动进程并完成 initialize 握手
    pub async fn spawn(exe: &PathBuf, driver_version: &str) -> Result<DriverHost, DriverError> {
        let mut child = Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| DriverError::Unavailable(e.to_string()))?;

        let stdin = child.stdin.take().ok_or_else(|| DriverError::Unavailable("无 stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| DriverError::Unavailable("无 stdout".into()))?;
        let stderr = child.stderr.take();

        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // 读任务：分发响应 / 处理通知
        {
            let pending = pending.clone();
            let dead = dead.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                    if let Some(id) = v.get("id").and_then(|x| x.as_u64()) {
                        let result = if let Some(err) = v.get("error") {
                            Err(DriverError::Remote {
                                code: err.get("code").and_then(|x| x.as_str()).unwrap_or("").into(),
                                kind: err.get("kind").and_then(|x| x.as_str()).unwrap_or("sql_error").into(),
                                message: err.get("message").and_then(|x| x.as_str()).unwrap_or("").into(),
                                retryable: err.get("retryable").and_then(|x| x.as_bool()).unwrap_or(false),
                            })
                        } else {
                            Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
                        };
                        if let Some(p) = pending.lock().await.remove(&id) {
                            let _ = p.tx.send(result);
                        }
                    } else {
                        // 通知（log）：进 tracing
                        let msg = v.get("params").and_then(|p| p.get("message")).and_then(|x| x.as_str()).unwrap_or("");
                        if !msg.is_empty() {
                            if cfg!(debug_assertions) {
                                eprintln!("[sql-driver] {msg}");
                            }
                        }
                    }
                }
                // stdout 关闭 = 驱动退出：全部 pending fail-fast
                dead.store(true, Ordering::SeqCst);
                let mut map = pending.lock().await;
                for (_, p) in map.drain() {
                    let _ = p.tx.send(Err(DriverError::Crashed));
                }
            });
        }

        // stderr → 日志（逐行）
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[sql-driver:stderr] {line}");
                }
            });
        }

        // 子进程退出监视（kill_on_drop 兜底；stdout EOF 已覆盖崩溃路径）
        let dead2 = dead.clone();
        let child = Arc::new(Mutex::new(child));
        {
            let child = child.clone();
            tokio::spawn(async move {
                let st = child.lock().await.wait().await;
                let _ = st;
                dead2.store(true, Ordering::SeqCst);
            });
        }

        let host = DriverHost {
            name: String::new(),
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            dead,
        };

        // initialize 握手（验证协议版本与驱动可用性）
        let init = host.call("initialize", serde_json::json!({
            "protocolVersion": 1,
            "agentVersion": env!("CARGO_PKG_VERSION"),
            "driverVersion": driver_version,
        }), INIT_DEADLINE).await?;
        let name = init.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();

        Ok(DriverHost { name, ..host })
    }

    /// 发起 RPC 调用（超时返回 Timeout；进程退出返回 Crashed）
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        deadline: Duration,
    ) -> Result<serde_json::Value, DriverError> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(DriverError::Crashed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, Pending { tx });

        let line = serde_json::json!({ "id": id, "method": method, "params": params }).to_string() + "\n";
        {
            let mut stdin = self.stdin.lock().await;
            if stdin.write_all(line.as_bytes()).await.is_err() {
                self.pending.lock().await.remove(&id);
                self.dead.store(true, Ordering::SeqCst);
                return Err(DriverError::Crashed);
            }
            let _ = stdin.flush().await;
        }

        match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(DriverError::Crashed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(DriverError::Timeout { method: method.to_string() })
            }
        }
    }

    /// 优雅关闭：发 disconnect，1s 后杀进程
    pub async fn shutdown(&self) {
        let _ = self.call("disconnect", serde_json::json!({}), Duration::from_secs(1)).await;
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        self.dead.store(true, Ordering::SeqCst);
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }
}
