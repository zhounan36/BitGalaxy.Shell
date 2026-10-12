//! Agent 接入面④：MCP server（stdio，docs/ai-design.md §2）。
//! 由 AI 客户端作为子进程拉起（bgshell-desktop --mcp-stdio）；
//! stdout 专用于 MCP 协议（newline-delimited JSON-RPC），诊断只走 stderr。
//! elicitation（server→client 审批请求）手工实现：按 id 关联响应，120s 超时 fail-closed。
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

use crate::metrics;
use crate::policy::{classify_command, AiPolicy, PolicyAction};
use crate::session::probe_tcp;
use crate::sftp::SftpController;
use crate::ssh::{HostKeyAsk, HostKeyCtx, SshConnection, SshError, SshKind};
use crate::stores::Stores;

// ── 工具错误码（稳定，AI 可据此自纠）─────────────────────────────────────

pub const ERR_HOST_NOT_FOUND: &str = "host-not-found";
pub const ERR_HOSTKEY_UNKNOWN: &str = "hostkey-unknown";
pub const ERR_HOSTKEY_MISMATCH: &str = "hostkey-mismatch";
pub const ERR_AUTH_FAILED: &str = "auth-failed";
pub const ERR_CONNECT_FAILED: &str = "connect-failed";
pub const ERR_SFTP_FAILED: &str = "sftp-failed";
pub const ERR_EXEC_FAILED: &str = "exec-failed";
pub const ERR_EXEC_TIMEOUT: &str = "exec-timeout";
pub const ERR_POLICY_DENIED: &str = "policy-denied";
pub const ERR_ELICIT_DECLINED: &str = "elicit-declined";
pub const ERR_ELICIT_UNSUPPORTED: &str = "elicit-unsupported";
pub const ERR_DISABLED: &str = "tool-disabled";

const MAX_WRITE_BYTES: usize = 1024 * 1024;
const ELICIT_TIMEOUT_MS: u64 = 120_000;

pub struct ToolError {
    pub code: &'static str,
    pub message: String,
}

impl ToolError {
    fn new(code: &'static str, message: impl Into<String>) -> ToolError {
        ToolError { code, message: message.into() }
    }
}

/// 拒绝类错误 → 审计决策字段
fn decision_of(code: &str) -> &'static str {
    match code {
        ERR_POLICY_DENIED => "policy-denied",
        ERR_ELICIT_DECLINED => "elicit-declined",
        ERR_ELICIT_UNSUPPORTED => "elicit-unsupported",
        ERR_DISABLED => "tool-disabled",
        _ => "allow",
    }
}

// ── SSH 连接池（按 hostId 复用；TOFU fail-closed：未知指纹直接拒绝）──────

struct PoolEntry {
    conn: Arc<SshConnection>,
    sftp: Option<Arc<russh_sftp::client::SftpSession>>,
    idle_task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct McpHostPool {
    stores: Stores,
    idle_close_sec: u64,
    entries: Arc<StdMutex<HashMap<String, PoolEntry>>>,
    locks: Arc<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl McpHostPool {
    fn new(stores: Stores, idle_close_sec: u64) -> McpHostPool {
        McpHostPool {
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

    fn evict(&self, host_id: &str, reason: &str) {
        if let Some(entry) = self.entries.lock().unwrap().remove(host_id) {
            entry.idle_task.abort();
            let conn = entry.conn.clone();
            let sftp = entry.sftp.clone();
            tokio::spawn(async move {
                if let Some(s) = sftp {
                    let _ = s.close().await;
                }
                conn.close().await;
            });
            let _ = reason;
        }
    }

    fn close_all(&self) {
        let ids: Vec<String> = self.entries.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.evict(&id, "server-stopped");
        }
    }

    async fn acquire(&self, host_id: &str) -> Result<Arc<SshConnection>, ToolError> {
        if let Some(entry) = self.entries.lock().unwrap().get(host_id) {
            let conn = entry.conn.clone();
            self.touch_idle(host_id);
            return Ok(conn);
        }
        let lock = self.host_lock(host_id);
        let _g = lock.lock().await;
        if let Some(entry) = self.entries.lock().unwrap().get(host_id) {
            let conn = entry.conn.clone();
            self.touch_idle(host_id);
            return Ok(conn);
        }

        let host = self
            .stores
            .hosts
            .get(host_id)
            .ok_or_else(|| ToolError::new(ERR_HOST_NOT_FOUND, format!("主机档案不存在: {host_id}")))?;
        let mismatch = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = Arc::new(HostKeyCtx {
            known: self.stores.known.clone(),
            host: host.input.host.clone(),
            port: host.input.port,
            ask: HostKeyAsk::FailClosed,
            mismatch: mismatch.clone(),
        });
        match crate::ssh::connect(&host, &SshKind::Exec, ctx).await {
            Ok(conn) => {
                let conn = Arc::new(conn);
                self.stores.audit.append(
                    "ai.connection-opened",
                    serde_json::json!({ "hostId": host_id, "target": conn.host_label }),
                );
                self.insert(host_id, conn.clone(), None);
                Ok(conn)
            }
            Err(e) => {
                if mismatch.load(Ordering::Relaxed) {
                    return Err(ToolError::new(
                        ERR_HOSTKEY_MISMATCH,
                        format!("主机 {}:{} 指纹不匹配（疑似中间人攻击），已拒绝连接", host.input.host, host.input.port),
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
                    SshError::Auth(msg) => {
                        Err(ToolError::new(ERR_AUTH_FAILED, format!("认证失败（{}@{}）: {msg}", host.input.username, host.input.host)))
                    }
                    SshError::Timeout | SshError::Connect { .. } => {
                        Err(ToolError::new(ERR_CONNECT_FAILED, e.message()))
                    }
                    SshError::Other(msg) => Err(ToolError::new(ERR_CONNECT_FAILED, msg)),
                }
            }
        }
    }

    async fn acquire_sftp(&self, host_id: &str) -> Result<Arc<russh_sftp::client::SftpSession>, ToolError> {
        if let Some(entry) = self.entries.lock().unwrap().get(host_id) {
            if let Some(sftp) = entry.sftp.clone() {
                self.touch_idle(host_id);
                return Ok(sftp);
            }
        }
        let conn = self.acquire(host_id).await?;
        let sftp = conn
            .open_sftp()
            .await
            .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("SFTP 通道打开失败: {e}")))?;
        let sftp = Arc::new(sftp);
        if let Some(entry) = self.entries.lock().unwrap().get_mut(host_id) {
            entry.sftp = Some(sftp.clone());
        }
        self.touch_idle(host_id);
        Ok(sftp)
    }

    async fn acquire_sftp_ctrl(&self, host_id: &str) -> Result<Arc<SftpController>, ToolError> {
        Ok(Arc::new(SftpController::from_shared(self.acquire_sftp(host_id).await?)))
    }

    fn insert(
        &self,
        host_id: &str,
        conn: Arc<SshConnection>,
        sftp: Option<Arc<russh_sftp::client::SftpSession>>,
    ) {
        let pool = self.clone();
        let id = host_id.to_string();
        let idle_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(pool.idle_close_sec.max(1))).await;
            pool.evict(&id, "idle-timeout");
        });
        self.entries
            .lock()
            .unwrap()
            .insert(host_id.to_string(), PoolEntry { conn, sftp, idle_task });
    }

    fn touch_idle(&self, host_id: &str) {
        let pool = self.clone();
        let id = host_id.to_string();
        let idle_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(pool.idle_close_sec.max(1))).await;
            pool.evict(&id, "idle-timeout");
        });
        if let Some(entry) = self.entries.lock().unwrap().get_mut(host_id) {
            entry.idle_task.abort();
            entry.idle_task = idle_task;
        }
    }
}

// ── MCP stdio 主循环 ─────────────────────────────────────────────────────

struct McpState {
    stores: Stores,
    policy: AiPolicy,
    pool: McpHostPool,
    client_label: String,
    elicit_supported: bool,
    next_id: AtomicI64,
    pending: StdMutex<HashMap<i64, oneshot::Sender<serde_json::Value>>>,
    stdout: tokio::io::Stdout,
}

/// stdio MCP 入口（main.rs 在 Tauri 初始化前调用；自带 tokio runtime）
pub fn run_stdio(data_dir: &Path) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(run_stdio_async(data_dir));
}

async fn run_stdio_async(data_dir: &Path) {
    let db = crate::db::Db::open(&data_dir.join("bitgalaxy.db"));
    db.migrate_from_json(data_dir);
    let stores = Stores::open(db);
    let policy = crate::policy::load_ai_policy(data_dir, &stores);
    let pool = McpHostPool::new(stores.clone(), policy.limits.idle_close_sec);

    let mut state = McpState {
        stores: stores.clone(),
        policy,
        pool,
        client_label: "unknown".into(),
        elicit_supported: false,
        next_id: AtomicI64::new(1),
        pending: StdMutex::new(HashMap::new()),
        stdout: tokio::io::stdout(),
    };

    stores.audit.append(
        "ai.server-started",
        serde_json::json!({
            "policy": {
                "execEnabled": state.policy.exec.enabled,
                "execDefault": state.policy.exec.default_action,
                "writeEnabled": state.policy.write.enabled,
                "writeDefault": state.policy.write.default_action,
            }
        }),
    );
    eprintln!(
        "[bgshell-mcp] ready (exec={:?}, write={:?})",
        state.policy.exec.default_action, state.policy.write.default_action
    );

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if msg.get("method").is_some() && msg.get("id").is_some() {
            handle_request(&mut state, msg).await; // 客户端请求
        } else if msg.get("method").is_some() {
            // 通知（notifications/initialized 等）：忽略
        } else if let Some(id) = msg.get("id").cloned() {
            resolve_pending(&state, id, msg); // 对我们 elicitation 请求的响应
        }
    }

    // stdio 关闭（客户端退出）→ 关闭全部连接池并退出进程
    state.pool.close_all();
    state.stores.audit.append("ai.server-stopped", serde_json::json!({}));
    std::process::exit(0);
}

fn resolve_pending(state: &McpState, id: serde_json::Value, msg: serde_json::Value) {
    if let Some(id_num) = id.as_i64() {
        if let Some(tx) = state.pending.lock().unwrap().remove(&id_num) {
            let _ = tx.send(msg);
        }
    }
}

async fn send_json(state: &mut McpState, value: &serde_json::Value) {
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    let _ = state.stdout.write_all(line.as_bytes()).await;
    let _ = state.stdout.flush().await;
}

async fn send_result(state: &mut McpState, id: &serde_json::Value, result: serde_json::Value) {
    send_json(
        state,
        &serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
    .await;
}

async fn send_error(state: &mut McpState, id: &serde_json::Value, code: i64, message: &str) {
    send_json(
        state,
        &serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    )
    .await;
}

async fn handle_request(state: &mut McpState, msg: serde_json::Value) {
    let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let params = msg.get("params").cloned().unwrap_or(serde_json::Value::Null);
    match method.as_str() {
        "initialize" => {
            if let Some(info) = params.pointer("/clientInfo") {
                let name = info.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                let version = info.get("version").and_then(|v| v.as_str()).unwrap_or("");
                state.client_label = format!("{name}/{version}");
            }
            state.elicit_supported = params.pointer("/capabilities/elicitation").is_some();
            let requested = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05")
                .to_string();
            send_result(
                state,
                &id,
                serde_json::json!({
                    "protocolVersion": requested,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "bgshell", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await;
        }
        "tools/list" => {
            send_result(state, &id, serde_json::json!({ "tools": tool_defs() })).await;
        }
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let args = params.get("arguments").cloned().unwrap_or(serde_json::json!({}));
            let result = call_tool(state, &name, &args).await;
            send_result(state, &id, result).await;
        }
        "ping" => {
            send_result(state, &id, serde_json::json!({})).await;
        }
        other => {
            send_error(state, &id, -32601, &format!("未知方法: {other}")).await;
        }
    }
}

/// server→client 审批请求（elicitation/create）；不支持/超时/拒绝 均 fail-closed
async fn elicit_approval(state: &mut McpState, message: &str) -> Result<bool, ToolError> {
    if !state.elicit_supported {
        return Err(ToolError::new(ERR_ELICIT_UNSUPPORTED, String::new()));
    }
    let req_id = state.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel::<serde_json::Value>();
    state.pending.lock().unwrap().insert(req_id, tx);
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "elicitation/create",
        "params": {
            "message": message,
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "approve": {
                        "type": "boolean",
                        "title": "允许执行",
                        "description": "勾选并提交以允许 AI 执行该操作"
                    }
                },
                "required": ["approve"]
            }
        }
    });
    send_json(state, &request).await;
    match tokio::time::timeout(Duration::from_millis(ELICIT_TIMEOUT_MS), rx).await {
        Ok(Ok(resp)) => {
            let action = resp.pointer("/result/action").and_then(|v| v.as_str()).unwrap_or("decline");
            let approved = resp
                .pointer("/result/content/approve")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Ok(action == "accept" && approved)
        }
        _ => Ok(false),
    }
}

/// 写操作统一闸门：write.enabled → defaultAction
async fn guard_write(state: &mut McpState, _host_id: &str, describe: &str) -> Result<(), ToolError> {
    if !state.policy.write.enabled {
        return Err(ToolError::new(
            ERR_DISABLED,
            "写操作已被策略禁用（ai-policy.json write.enabled=false）",
        ));
    }
    match state.policy.write.default_action {
        PolicyAction::Deny => Err(ToolError::new(
            ERR_POLICY_DENIED,
            "写操作被默认拒绝（ai-policy.json write.defaultAction=deny）",
        )),
        PolicyAction::Ask => {
            let approved = elicit_approval(state, &format!("允许 AI 执行写操作？\n\n{describe}")).await?;
            if approved {
                Ok(())
            } else {
                Err(ToolError::new(ERR_ELICIT_DECLINED, "用户拒绝了该写操作"))
            }
        }
        PolicyAction::Allow => Ok(()),
    }
}

// ── 工具定义 ─────────────────────────────────────────────────────────────

fn host_id_schema() -> serde_json::Value {
    serde_json::json!({ "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" } })
}

fn tool_defs() -> serde_json::Value {
    serde_json::json!([
        { "name": "list_hosts", "title": "列出主机",
          "description": "列出 BitGalaxy Shell 主机档案（脱敏：不含任何凭据）。返回 id、名称、地址、用户、标签与 hostKeyKnown（指纹是否已确认；false 的主机需先在工作台人工连接一次）。",
          "inputSchema": { "type": "object", "properties": {} },
          "annotations": { "readOnlyHint": true } },
        { "name": "probe_host", "title": "探测主机连通性",
          "description": "对目标主机做 TCP 端口探测（不做 SSH 握手，不使用凭据），返回延迟或失败原因。",
          "inputSchema": { "type": "object", "properties": host_id_schema(), "required": ["hostId"] },
          "annotations": { "readOnlyHint": true } },
        { "name": "get_metrics", "title": "采集主机运行指标",
          "description": "采集一次主机运行指标（uptime、负载、内存、磁盘、top 进程）。适用于 Linux 目标；主机需已在已知指纹列表。",
          "inputSchema": { "type": "object", "properties": host_id_schema(), "required": ["hostId"] },
          "annotations": { "readOnlyHint": true } },
        { "name": "list_dir", "title": "列远程目录",
          "description": "SFTP 列出远程目录内容（名称/类型/大小/mtime）。path 支持相对路径（相对登录用户家目录）。",
          "inputSchema": { "type": "object", "properties": {
              "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
              "path": { "type": "string", "description": "远程目录路径" } }, "required": ["hostId", "path"] },
          "annotations": { "readOnlyHint": true } },
        { "name": "read_file", "title": "读远程文本文件",
          "description": "SFTP 读取远程文本文件（UTF-8）。默认最多读取 256KB（受策略钳制），超限返回截断内容并标注 truncated 与 sizeB。",
          "inputSchema": { "type": "object", "properties": {
              "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
              "path": { "type": "string", "description": "远程文件路径" },
              "offset": { "type": "integer", "minimum": 0, "description": "起始字节偏移（默认 0）" },
              "maxBytes": { "type": "integer", "minimum": 1, "description": "本次最多读取字节数（受策略上限钳制）" } },
              "required": ["hostId", "path"] },
          "annotations": { "readOnlyHint": true } },
        { "name": "exec_command", "title": "执行远程命令",
          "description": "在目标主机上执行一次性非交互命令（ssh exec 通道），返回 stdout/stderr/退出码。命令经安全策略分级：内置黑名单直接拒绝；白名单直接放行；其余按 ai-policy.json defaultAction（默认需用户在客户端审批）。超时默认 30s（可传 timeoutMs，受策略上限钳制），输出超 64KB 截断。",
          "inputSchema": { "type": "object", "properties": {
              "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
              "command": { "type": "string", "description": "要执行的命令（非交互；交互式命令会因 stdin 关闭而失败）" },
              "timeoutMs": { "type": "integer", "minimum": 1000, "description": "超时毫秒数（受策略上限钳制）" } },
              "required": ["hostId", "command"] } },
        { "name": "write_file", "title": "写远程文本文件",
          "description": "SFTP 写入远程文本文件（UTF-8，最大 1MB）。mode=overwrite 覆盖 / append 追加。受写策略与审批约束。",
          "inputSchema": { "type": "object", "properties": {
              "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
              "path": { "type": "string", "description": "远程文件路径" },
              "content": { "type": "string", "description": "写入内容" },
              "mode": { "type": "string", "enum": ["overwrite", "append"], "description": "默认 overwrite" } },
              "required": ["hostId", "path", "content"] } },
        { "name": "make_dir", "title": "建远程目录",
          "description": "SFTP 创建远程目录（单级，等价 mkdir）。受写策略与审批约束。",
          "inputSchema": { "type": "object", "properties": {
              "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
              "path": { "type": "string", "description": "远程目录路径" } }, "required": ["hostId", "path"] } },
        { "name": "rename_path", "title": "改名/移动远程路径",
          "description": "SFTP 改名或移动远程文件/目录。受写策略与审批约束。",
          "inputSchema": { "type": "object", "properties": {
              "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
              "from": { "type": "string", "description": "原路径" },
              "to": { "type": "string", "description": "目标路径" } }, "required": ["hostId", "from", "to"] } },
        { "name": "delete_path", "title": "删除远程路径",
          "description": "SFTP 删除远程文件；recursive=true 删除整个目录树（危险）。受写策略与审批约束；递归删除根目录/家目录被硬性拒绝。",
          "inputSchema": { "type": "object", "properties": {
              "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
              "path": { "type": "string", "description": "远程路径" },
              "recursive": { "type": "boolean", "description": "递归删除目录树" } },
              "required": ["hostId", "path"] },
          "annotations": { "destructiveHint": true } },
    ])
}

// ── 工具执行（统一审计）──────────────────────────────────────────────────

fn text_payload(payload: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "content": [ { "type": "text", "text": payload.to_string() } ] })
}

fn err_payload(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "content": [ { "type": "text", "text": serde_json::json!({ "ok": false, "code": code, "message": message }).to_string() } ],
        "isError": true
    })
}

fn host_label_of(stores: &Stores, host_id: &str) -> String {
    match stores.hosts.get(host_id) {
        Some(h) => format!("{}（{}@{}:{}）", h.input.name, h.input.username, h.input.host, h.input.port),
        None => host_id.to_string(),
    }
}

async fn call_tool(state: &mut McpState, name: &str, args: &serde_json::Value) -> serde_json::Value {
    let started = std::time::Instant::now();
    let host_id = args.get("hostId").and_then(|v| v.as_str()).map(|s| s.to_string());

    let mut detail = serde_json::Map::new();
    let outcome: Result<serde_json::Value, ToolError> = match name {
        "list_hosts" => Ok(run_list_hosts(state)),
        "probe_host" => run_probe_host(state, args).await,
        "get_metrics" => run_get_metrics(state, args, &mut detail).await,
        "list_dir" => run_list_dir(state, args, &mut detail).await,
        "read_file" => run_read_file(state, args, &mut detail).await,
        "exec_command" => run_exec_command(state, args, &mut detail).await,
        "write_file" => run_write_file(state, args, &mut detail).await,
        "make_dir" => run_make_dir(state, args, &mut detail).await,
        "rename_path" => run_rename_path(state, args, &mut detail).await,
        "delete_path" => run_delete_path(state, args, &mut detail).await,
        _ => Err(ToolError::new("unknown-tool", format!("未知工具: {name}"))),
    };

    // 先落审计再回结果
    let mut audit_meta = serde_json::json!({
        "tool": name,
        "hostId": host_id,
        "client": state.client_label,
        "ms": started.elapsed().as_millis() as u64,
    });
    match &outcome {
        Ok(_) => {}
        Err(e) => {
            audit_meta["decision"] = serde_json::json!(decision_of(e.code));
            audit_meta["error"] = serde_json::json!(e.code);
        }
    }
    if let Some(obj) = audit_meta.as_object_mut() {
        for (k, v) in detail {
            obj.insert(k, v);
        }
    }
    state.stores.audit.append("ai.tool", audit_meta);

    match outcome {
        Ok(mut payload) => {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("ok".into(), serde_json::json!(true));
            }
            text_payload(payload)
        }
        Err(e) => err_payload(e.code, &e.message),
    }
}

fn run_list_hosts(state: &McpState) -> serde_json::Value {
    let hosts: Vec<serde_json::Value> = state
        .stores
        .hosts
        .list()
        .iter()
        .map(|h| {
            serde_json::json!({
                "id": h.id,
                "name": h.input.name,
                "host": h.input.host,
                "port": h.input.port,
                "username": h.input.username,
                "tags": h.input.tags.clone().unwrap_or_default(),
                "hostKeyKnown": state.stores.known.find(&h.input.host, h.input.port).is_some(),
            })
        })
        .collect();
    serde_json::json!({ "hosts": hosts })
}

async fn run_probe_host(state: &McpState, args: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let host = state
        .stores
        .hosts
        .get(host_id)
        .ok_or_else(|| ToolError::new(ERR_HOST_NOT_FOUND, format!("主机档案不存在: {host_id}")))?;
    let r = probe_tcp(&host.input.host, host.input.port).await;
    Ok(serde_json::json!({ "ok": r.ok, "ms": r.ms, "message": r.message }))
}

async fn run_get_metrics(
    state: &mut McpState,
    args: &serde_json::Value,
    _detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let conn = state.pool.acquire(host_id).await?;
    let timeout = state.policy.limits.exec_timeout_ms.min(15_000);
    let r = conn
        .exec(&metrics::build_metrics_command(), timeout, state.policy.limits.max_exec_output_bytes)
        .await
        .map_err(|e| ToolError::new(ERR_EXEC_FAILED, e))?;
    if r.timed_out {
        return Err(ToolError::new(ERR_EXEC_TIMEOUT, "指标采集超时"));
    }
    let sample = metrics::parse_metrics(&r.stdout);
    Ok(serde_json::json!({ "sample": serde_json::to_value(sample).unwrap_or_default() }))
}

async fn run_list_dir(
    state: &mut McpState,
    args: &serde_json::Value,
    _detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let ctrl = state.pool.acquire_sftp_ctrl(host_id).await?;
    let (resolved, entries) = ctrl
        .ls(path)
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    Ok(serde_json::json!({
        "resolvedPath": resolved,
        "entries": serde_json::to_value(entries).unwrap_or_default()
    }))
}

async fn run_read_file(
    state: &mut McpState,
    args: &serde_json::Value,
    _detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default();
    let requested = args.get("maxBytes").and_then(|v| v.as_u64()).unwrap_or(0);
    let max_bytes = requested.clamp(1, state.policy.limits.max_read_file_bytes as u64) as usize;
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);

    let sftp = state.pool.acquire_sftp(host_id).await?;
    let meta = sftp
        .metadata(path)
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    let size = meta.size.unwrap_or(0);
    if size == 0 {
        return Ok(serde_json::json!({ "path": path, "offset": offset, "content": "", "sizeB": 0, "truncated": false }));
    }
    let mut file = sftp
        .open(path)
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    let mut buf: Vec<u8> = Vec::with_capacity(max_bytes.min(size as usize));
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut chunk)
            .await
            .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() >= max_bytes {
            buf.truncate(max_bytes);
            break;
        }
    }
    let read = buf.len() as u64;
    Ok(serde_json::json!({
        "path": path,
        "offset": offset,
        "content": String::from_utf8_lossy(&buf),
        "sizeB": size,
        "truncated": offset + read < size,
    }))
}

async fn run_exec_command(
    state: &mut McpState,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let command = args.get("command").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    detail.insert("command".into(), serde_json::json!(command));

    if !state.policy.exec.enabled {
        return Err(ToolError::new(ERR_DISABLED, "命令执行已被策略禁用（ai-policy.json exec.enabled=false）"));
    }
    let cls = classify_command(&command, &state.policy);
    match cls.action {
        PolicyAction::Deny => {
            let pattern = cls.pattern.unwrap_or_default();
            let which = if cls.reason == "deny-pattern" { "黑名单" } else { "默认拒绝" };
            return Err(ToolError::new(
                ERR_POLICY_DENIED,
                format!("命令被安全策略拒绝（命中{which}规则: {pattern}）"),
            ));
        }
        PolicyAction::Ask => {
            let approved = elicit_approval(
                state,
                &format!("允许 AI 在主机 {} 上执行命令？\n\n{}", host_label_of(&state.stores, host_id), command),
            )
            .await?;
            if !approved {
                return Err(ToolError::new(ERR_ELICIT_DECLINED, "用户拒绝了该命令执行"));
            }
        }
        PolicyAction::Allow => {}
    }
    let requested = args.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(0);
    let timeout = requested
        .clamp(1000, state.policy.limits.exec_timeout_ms);
    let conn = state.pool.acquire(host_id).await?;
    let r = conn
        .exec(&command, timeout, state.policy.limits.max_exec_output_bytes)
        .await
        .map_err(|e| ToolError::new(ERR_EXEC_FAILED, e))?;
    detail.insert("exitCode".into(), serde_json::json!(r.code));
    Ok(serde_json::json!({
        "exitCode": r.code,
        "stdout": r.stdout,
        "stderr": r.stderr,
        "truncated": r.truncated,
        "timedOut": r.timed_out,
    }))
}

async fn run_write_file(
    state: &mut McpState,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default();
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or_default();
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("overwrite").to_string();
    detail.insert("path".into(), serde_json::json!(path));
    detail.insert("mode".into(), serde_json::json!(mode));

    let bytes = content.as_bytes();
    if bytes.len() > MAX_WRITE_BYTES {
        return Err(ToolError::new(ERR_POLICY_DENIED, format!("内容超过 {MAX_WRITE_BYTES} 字节上限，拒绝写入")));
    }
    let describe = format!(
        "主机 {}\n路径: {path}（{mode}，{} 字节）",
        host_label_of(&state.stores, host_id),
        bytes.len()
    );
    guard_write(state, host_id, &describe).await?;

    let sftp = state.pool.acquire_sftp(host_id).await?;
    use tokio::io::AsyncWriteExt;
    let flags_append = russh_sftp::protocol::OpenFlags::WRITE
        | russh_sftp::protocol::OpenFlags::CREATE
        | russh_sftp::protocol::OpenFlags::APPEND;
    let opened = if mode == "append" {
        sftp.open_with_flags(path, flags_append).await
    } else {
        sftp.create(path).await
    };
    let write_result: Result<(), String> = match opened {
        Ok(mut file) => async move {
            file.write_all(bytes).await.map_err(|e| e.to_string())?;
            file.flush().await.map_err(|e| e.to_string())
        }
        .await
        .map_err(|e| e),
        Err(e) => Err(e.to_string()),
    };
    write_result.map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    Ok(serde_json::json!({ "path": path, "mode": mode, "bytes": bytes.len() }))
}

async fn run_make_dir(
    state: &mut McpState,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default();
    detail.insert("path".into(), serde_json::json!(path));
    let describe = format!("主机 {}\n新建目录: {path}", host_label_of(&state.stores, host_id));
    guard_write(state, host_id, &describe).await?;
    let ctrl = state.pool.acquire_sftp_ctrl(host_id).await?;
    ctrl.mkdir(path)
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    Ok(serde_json::json!({ "path": path }))
}

async fn run_rename_path(
    state: &mut McpState,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let from = args.get("from").and_then(|v| v.as_str()).unwrap_or_default();
    let to = args.get("to").and_then(|v| v.as_str()).unwrap_or_default();
    detail.insert("from".into(), serde_json::json!(from));
    detail.insert("to".into(), serde_json::json!(to));
    let describe = format!("主机 {}\n{from} → {to}", host_label_of(&state.stores, host_id));
    guard_write(state, host_id, &describe).await?;
    let ctrl = state.pool.acquire_sftp_ctrl(host_id).await?;
    ctrl.rename(from, to)
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("{from}: {e}")))?;
    Ok(serde_json::json!({ "from": from, "to": to }))
}

async fn run_delete_path(
    state: &mut McpState,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default();
    let recursive = args.get("recursive").and_then(|v| v.as_bool()).unwrap_or(false);
    detail.insert("path".into(), serde_json::json!(path));
    detail.insert("recursive".into(), serde_json::json!(recursive));

    let trimmed = path.trim();
    if recursive && (trimmed.is_empty() || trimmed == "/" || trimmed == "~" || trimmed == "/~" || trimmed == "~/") {
        return Err(ToolError::new(ERR_POLICY_DENIED, "拒绝递归删除根目录/家目录"));
    }
    let describe = format!(
        "主机 {}\n删除: {path}{}",
        host_label_of(&state.stores, host_id),
        if recursive { "（递归删除整个目录树）" } else { "" }
    );
    guard_write(state, host_id, &describe).await?;
    let ctrl = state.pool.acquire_sftp_ctrl(host_id).await?;
    ctrl.rm(path, recursive)
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    Ok(serde_json::json!({ "path": path, "recursive": recursive }))
}
