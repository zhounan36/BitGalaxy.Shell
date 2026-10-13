//! AI 工具核心层：`mcp.rs`（MCP stdio 通道）与 `ai.rs`（桌宠 LLM 通道）共享。
//! 职责：命令分类、15 个工具实现、审批抽象（[`Approver`]）与审计；
//! SSH/SFTP 连接统一走 `pool.rs` 的 [`ConnRegistry`]（与工作台共享）。
//! 通道层只负责协议适配（JSON-RPC / LLM function calling）与各自的审批 UI。
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::metrics;
use crate::policy::{classify_command, AiPolicy, PolicyAction};
use crate::pool::ConnRegistry;
use crate::session::probe_tcp;
use crate::sftp::SftpController;
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
// 本地工具限额
const LOCAL_EXEC_TIMEOUT_MS: u64 = 120_000;
const LOCAL_EXEC_TIMEOUT_MAX_MS: u64 = 600_000;
const LOCAL_EXEC_OUTPUT_MAX: usize = 64 * 1024;
const LOCAL_READ_MAX_BYTES: usize = 256 * 1024;
const TRANSFER_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024 - 1; // AI 单次传输上限 4GiB-1
/// 大文件阈值：≥1GB 触发确认（预检报告同样以此判定）
const LARGE_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const TRANSFER_CHUNK: usize = 64 * 1024;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct ToolError {
    pub code: &'static str,
    pub message: String,
}

impl ToolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> ToolError {
        ToolError { code, message: message.into() }
    }
}

/// 拒绝类错误 → 审计决策字段
pub fn decision_of(code: &str) -> &'static str {
    match code {
        ERR_POLICY_DENIED => "policy-denied",
        ERR_ELICIT_DECLINED => "elicit-declined",
        ERR_ELICIT_UNSUPPORTED => "elicit-unsupported",
        ERR_DISABLED => "tool-disabled",
        _ => "allow",
    }
}

// ── 审批抽象（通道各自实现 UI：elicitation / 桌宠弹卡片）──────────────────

/// 审批 future：实现方把所需状态 clone 进闭包，不借用外部生命周期
pub type ApproveFuture = Pin<Box<dyn Future<Output = Result<bool, ToolError>> + Send>>;

pub trait Approver: Send + Sync {
    /// 请求人工审批；Err 仅表示该通道不支持审批（fail-closed），false 表示被拒/超时
    fn approve(&self, describe: &str) -> ApproveFuture;
}

// ── 执行上下文与统一入口 ─────────────────────────────────────────────────

pub struct ToolCtx<'a> {
    pub stores: &'a Stores,
    pub policy: &'a AiPolicy,
    pub registry: &'a ConnRegistry,
    /// 审计标识：通道名（"pet" / MCP clientInfo）
    pub client: &'a str,
}

fn host_label_of(stores: &Stores, host_id: &str) -> String {
    match stores.hosts.get(host_id) {
        Some(h) => format!("{}（{}@{}:{}）", h.input.name, h.input.username, h.input.host, h.input.port),
        None => host_id.to_string(),
    }
}

/// 统一工具入口：分发 → 审计（ai.tool）→ 返回结果 JSON（含 ok:true）
pub async fn execute(
    ctx: &ToolCtx<'_>,
    approver: &dyn Approver,
    name: &str,
    args: &serde_json::Value,
) -> serde_json::Value {
    let started = Instant::now();
    let host_id = args.get("hostId").and_then(|v| v.as_str()).map(|s| s.to_string());

    let mut detail = serde_json::Map::new();
    let outcome = dispatch(ctx, approver, name, args, &mut detail).await;

    // 先落审计再回结果
    let mut audit_meta = serde_json::json!({
        "tool": name,
        "hostId": host_id,
        "client": ctx.client,
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
    ctx.stores.audit.append("ai.tool", audit_meta);

    match outcome {
        Ok(mut payload) => {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("ok".into(), serde_json::json!(true));
            }
            payload
        }
        Err(e) => serde_json::json!({ "ok": false, "code": e.code, "message": e.message }),
    }
}

async fn dispatch(
    ctx: &ToolCtx<'_>,
    approver: &dyn Approver,
    name: &str,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    match name {
        // 远程主机
        "list_hosts" => run_list_hosts(ctx),
        "probe_host" => run_probe_host(ctx, args).await,
        "get_metrics" => run_get_metrics(ctx, args).await,
        "list_dir" => run_list_dir(ctx, args).await,
        "read_file" => run_read_file(ctx, args).await,
        "exec_command" => run_exec_command(ctx, approver, args, detail).await,
        "write_file" => run_write_file(ctx, approver, args, detail).await,
        "make_dir" => run_make_dir(ctx, approver, args, detail).await,
        "rename_path" => run_rename_path(ctx, approver, args, detail).await,
        "delete_path" => run_delete_path(ctx, approver, args, detail).await,
        // 本机与传输
        "local_exec" => run_local_exec(ctx, approver, args, detail).await,
        "local_read_file" => run_local_read_file(ctx, args, detail).await,
        "local_write_file" => run_local_write_file(ctx, approver, args, detail).await,
        "sftp_upload_file" => run_sftp_transfer(ctx, approver, args, true, detail).await,
        "sftp_download_file" => run_sftp_transfer(ctx, approver, args, false, detail).await,
        _ => Err(ToolError::new("unknown-tool", format!("未知工具: {name}"))),
    }
}

/// 写操作统一闸门：write.enabled → defaultAction → 审批
async fn guard_write(ctx: &ToolCtx<'_>, approver: &dyn Approver, describe: &str) -> Result<(), ToolError> {
    if !ctx.policy.write.enabled {
        return Err(ToolError::new(
            ERR_DISABLED,
            "写操作已被策略禁用（ai-policy.json write.enabled=false）",
        ));
    }
    match ctx.policy.write.default_action {
        PolicyAction::Deny => Err(ToolError::new(
            ERR_POLICY_DENIED,
            "写操作被默认拒绝（ai-policy.json write.defaultAction=deny）",
        )),
        PolicyAction::Ask => {
            let approved = approver.approve(&format!("允许 AI 执行写操作？\n\n{describe}")).await?;
            if approved {
                Ok(())
            } else {
                Err(ToolError::new(ERR_ELICIT_DECLINED, "用户拒绝了该写操作"))
            }
        }
        PolicyAction::Allow => Ok(()),
    }
}

// ── 远程工具实现 ─────────────────────────────────────────────────────────

fn run_list_hosts(ctx: &ToolCtx<'_>) -> Result<serde_json::Value, ToolError> {
    let hosts: Vec<serde_json::Value> = ctx
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
                "hostKeyKnown": ctx.stores.known.find(&h.input.host, h.input.port).is_some(),
            })
        })
        .collect();
    Ok(serde_json::json!({ "hosts": hosts }))
}

async fn run_probe_host(ctx: &ToolCtx<'_>, args: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let host = ctx
        .stores
        .hosts
        .get(host_id)
        .ok_or_else(|| ToolError::new(ERR_HOST_NOT_FOUND, format!("主机档案不存在: {host_id}")))?;
    let r = probe_tcp(&host.input.host, host.input.port).await;
    Ok(serde_json::json!({ "ok": r.ok, "ms": r.ms, "message": r.message }))
}

async fn run_get_metrics(ctx: &ToolCtx<'_>, args: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let lease = ctx.registry.acquire_transient(host_id, ctx.client).await?;
    let timeout = ctx.policy.limits.exec_timeout_ms.min(15_000);
    let r = lease
        .conn
        .exec(&metrics::build_metrics_command(), timeout, ctx.policy.limits.max_exec_output_bytes)
        .await
        .map_err(|e| ToolError::new(ERR_EXEC_FAILED, e))?;
    if r.timed_out {
        return Err(ToolError::new(ERR_EXEC_TIMEOUT, "指标采集超时"));
    }
    let sample = metrics::parse_metrics(&r.stdout);
    Ok(serde_json::json!({ "sample": serde_json::to_value(sample).unwrap_or_default() }))
}

async fn run_list_dir(ctx: &ToolCtx<'_>, args: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let lease = ctx.registry.acquire_sftp(host_id, ctx.client).await?;
    let ctrl = SftpController::from_shared(lease.sftp.clone());
    let (resolved, entries) = ctrl
        .ls(path)
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    Ok(serde_json::json!({
        "resolvedPath": resolved,
        "entries": serde_json::to_value(entries).unwrap_or_default()
    }))
}

async fn run_read_file(ctx: &ToolCtx<'_>, args: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default();
    let requested = args.get("maxBytes").and_then(|v| v.as_u64()).unwrap_or(0);
    let max_bytes = requested.clamp(1, ctx.policy.limits.max_read_file_bytes as u64) as usize;
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);

    let lease = ctx.registry.acquire_sftp(host_id, ctx.client).await?;
    let sftp = &lease.sftp;
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
    use tokio::io::AsyncSeekExt;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    let mut buf: Vec<u8> = Vec::with_capacity(max_bytes.min(size as usize));
    let mut chunk = vec![0u8; TRANSFER_CHUNK];
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
    ctx: &ToolCtx<'_>,
    approver: &dyn Approver,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let command = args.get("command").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    detail.insert("command".into(), serde_json::json!(command));

    if !ctx.policy.exec.enabled {
        return Err(ToolError::new(ERR_DISABLED, "命令执行已被策略禁用（ai-policy.json exec.enabled=false）"));
    }
    let cls = classify_command(&command, ctx.policy);
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
            let approved = approver
                .approve(&format!(
                    "允许 AI 在主机 {} 上执行命令？\n\n{}",
                    host_label_of(ctx.stores, host_id),
                    command
                ))
                .await?;
            if !approved {
                return Err(ToolError::new(ERR_ELICIT_DECLINED, "用户拒绝了该命令执行"));
            }
        }
        PolicyAction::Allow => {}
    }
    let requested = args.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(0);
    let timeout = if requested == 0 {
        ctx.policy.limits.exec_timeout_ms.min(30_000)
    } else {
        requested.clamp(1000, ctx.policy.limits.exec_timeout_ms)
    };
    let lease = ctx.registry.acquire_transient(host_id, ctx.client).await?;
    let r = lease
        .conn
        .exec(&command, timeout, ctx.policy.limits.max_exec_output_bytes)
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
    ctx: &ToolCtx<'_>,
    approver: &dyn Approver,
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
        host_label_of(ctx.stores, host_id),
        bytes.len()
    );
    guard_write(ctx, approver, &describe).await?;

    let lease = ctx.registry.acquire_sftp(host_id, ctx.client).await?;
    let sftp = &lease.sftp;
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
            file.flush().await.map_err(|e| e.to_string())?;
            // 必须等待服务器确认关闭句柄：否则远端 sftp-server 仍持有写打开的 fd，
            // 紧接着执行 ./xx.sh 会报 Text file busy（Drop 的 close_nowait 不等确认）
            file.close().await.map_err(|e| e.to_string())
        }
        .await
        .map_err(|e| e),
        Err(e) => Err(e.to_string()),
    };
    write_result.map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    Ok(serde_json::json!({ "path": path, "mode": mode, "bytes": bytes.len() }))
}

async fn run_make_dir(
    ctx: &ToolCtx<'_>,
    approver: &dyn Approver,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default();
    detail.insert("path".into(), serde_json::json!(path));
    let describe = format!("主机 {}\n新建目录: {path}", host_label_of(ctx.stores, host_id));
    guard_write(ctx, approver, &describe).await?;
    let lease = ctx.registry.acquire_sftp(host_id, ctx.client).await?;
    let ctrl = SftpController::from_shared(lease.sftp.clone());
    ctrl.mkdir(path)
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    Ok(serde_json::json!({ "path": path }))
}

async fn run_rename_path(
    ctx: &ToolCtx<'_>,
    approver: &dyn Approver,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
    let from = args.get("from").and_then(|v| v.as_str()).unwrap_or_default();
    let to = args.get("to").and_then(|v| v.as_str()).unwrap_or_default();
    detail.insert("from".into(), serde_json::json!(from));
    detail.insert("to".into(), serde_json::json!(to));
    let describe = format!("主机 {}\n{from} → {to}", host_label_of(ctx.stores, host_id));
    guard_write(ctx, approver, &describe).await?;
    let lease = ctx.registry.acquire_sftp(host_id, ctx.client).await?;
    let ctrl = SftpController::from_shared(lease.sftp.clone());
    ctrl.rename(from, to)
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("{from}: {e}")))?;
    Ok(serde_json::json!({ "from": from, "to": to }))
}

async fn run_delete_path(
    ctx: &ToolCtx<'_>,
    approver: &dyn Approver,
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
        host_label_of(ctx.stores, host_id),
        if recursive { "（递归删除整个目录树）" } else { "" }
    );
    guard_write(ctx, approver, &describe).await?;
    let lease = ctx.registry.acquire_sftp(host_id, ctx.client).await?;
    let ctrl = SftpController::from_shared(lease.sftp.clone());
    ctrl.rm(path, recursive)
        .await
        .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("路径 {path}: {e}")))?;
    Ok(serde_json::json!({ "path": path, "recursive": recursive }))
}

// ── 本机与传输工具实现 ───────────────────────────────────────────────────

async fn run_local_exec(
    _ctx: &ToolCtx<'_>,
    approver: &dyn Approver,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let command = args.get("command").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    detail.insert("command".into(), serde_json::json!(command));
    if command.trim().is_empty() {
        return Err(ToolError::new(ERR_POLICY_DENIED, "命令为空"));
    }
    let cls = classify_local_command(&command);
    match cls {
        "deny" => return Err(ToolError::new(ERR_POLICY_DENIED, "本地命令命中安全黑名单，已拒绝执行")),
        "allow" => {}
        _ => {
            let approved = approver
                .approve(&format!("允许 AI 在本机（Windows）执行命令？\n\n{command}"))
                .await?;
            if !approved {
                return Err(ToolError::new(ERR_ELICIT_DECLINED, "用户拒绝了该本地命令"));
            }
        }
    }
    let timeout_sec = args
        .get("timeoutSec")
        .and_then(|v| v.as_f64())
        .unwrap_or(LOCAL_EXEC_TIMEOUT_MS as f64 / 1000.0);
    let timeout_ms = (timeout_sec * 1000.0).clamp(5_000.0, LOCAL_EXEC_TIMEOUT_MAX_MS as f64) as u64;
    let r = run_local_command(&command, timeout_ms)
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

async fn run_local_read_file(
    _ctx: &ToolCtx<'_>,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
    let requested = args.get("maxBytes").and_then(|v| v.as_u64()).unwrap_or(0);
    let max_bytes = requested.clamp(1, LOCAL_READ_MAX_BYTES as u64) as usize;
    detail.insert("path".into(), serde_json::json!(path));
    if path.trim().is_empty() {
        return Err(ToolError::new(ERR_POLICY_DENIED, "路径为空"));
    }
    // 凭据面路径硬拒（私钥/密钥文件不进模型上下文）
    let sensitive = Regex::new(r"(?i)([\\/]|^)(\.ssh[\\/]|id_(rsa|ed25519)|\.env$|credentials?|secret)").unwrap();
    if sensitive.is_match(&path) {
        return Err(ToolError::new(ERR_POLICY_DENIED, "该路径属于凭据敏感路径（私钥/密钥/凭据文件），拒绝读取"));
    }
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| ToolError::new(ERR_EXEC_FAILED, format!("无法访问 {path}: {e}")))?;
    if meta.is_dir() {
        return Err(ToolError::new(ERR_POLICY_DENIED, "路径是目录，请用 local_exec 的 Get-ChildItem 列目录"));
    }
    let size = meta.len();
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ToolError::new(ERR_EXEC_FAILED, format!("打开失败 {path}: {e}")))?;
    use tokio::io::AsyncSeekExt;
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| ToolError::new(ERR_EXEC_FAILED, format!("定位失败: {e}")))?;
    }
    let mut buf = Vec::with_capacity(max_bytes.min(size as usize));
    let mut chunk = vec![0u8; TRANSFER_CHUNK];
    loop {
        let n = file
            .read(&mut chunk)
            .await
            .map_err(|e| ToolError::new(ERR_EXEC_FAILED, format!("读取失败: {e}")))?;
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

async fn run_local_write_file(
    ctx: &ToolCtx<'_>,
    approver: &dyn Approver,
    args: &serde_json::Value,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or_default();
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("overwrite").to_string();
    detail.insert("path".into(), serde_json::json!(path));
    detail.insert("mode".into(), serde_json::json!(mode));
    if path.trim().is_empty() {
        return Err(ToolError::new(ERR_POLICY_DENIED, "路径为空"));
    }
    let bytes = content.as_bytes();
    if bytes.len() > MAX_WRITE_BYTES {
        return Err(ToolError::new(ERR_POLICY_DENIED, format!("内容超过 {MAX_WRITE_BYTES} 字节上限，请改用 sftp_upload_file 传输")));
    }
    // 系统关键路径硬拒
    let sys_path = Regex::new(r"(?i)^[a-z]:\\(windows|program files|program files \(x86\))\\|^\\\\").unwrap();
    if sys_path.is_match(&path) {
        return Err(ToolError::new(ERR_POLICY_DENIED, "系统关键路径禁止 AI 写入"));
    }
    let describe = format!("写本机文件: {path}（{mode}，{} 字节）", bytes.len());
    guard_write(ctx, approver, &describe).await?;

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(mode == "append")
        .truncate(mode != "append")
        .open(&path)
        .await
        .map_err(|e| ToolError::new(ERR_EXEC_FAILED, format!("打开失败 {path}: {e}")))?;
    file.write_all(bytes)
        .await
        .map_err(|e| ToolError::new(ERR_EXEC_FAILED, format!("写入失败: {e}")))?;
    Ok(serde_json::json!({ "path": path, "mode": mode, "bytes": bytes.len() }))
}

/// AI 传输工具（sftp_upload_file / sftp_download_file）：
/// 预检（重名/权限/空间/超大文件）→ 对话内确认（needConfirm）→ 用户批准 → 复用原生引擎（断点续传）。
async fn run_sftp_transfer(
    ctx: &ToolCtx<'_>,
    approver: &dyn Approver,
    args: &serde_json::Value,
    upload: bool,
    detail: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ToolError> {
    let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let local_path = args.get("localPath").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let remote_path = args.get("remotePath").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let overwrite = args.get("overwrite").and_then(|v| v.as_bool()).unwrap_or(false);
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    detail.insert("localPath".into(), serde_json::json!(local_path));
    detail.insert("remotePath".into(), serde_json::json!(remote_path));
    detail.insert("overwrite".into(), serde_json::json!(overwrite));
    if local_path.trim().is_empty() || remote_path.trim().is_empty() {
        return Err(ToolError::new(ERR_POLICY_DENIED, "localPath 与 remotePath 均不能为空"));
    }
    let ctx_host_id = host_id.clone();
    let _ = ctx.stores.hosts.get(&ctx_host_id)
        .ok_or_else(|| ToolError::new(ERR_HOST_NOT_FOUND, format!("主机档案不存在: {host_id}")))?;
    let direction_label = if upload { "本机 → 服务器" } else { "服务器 → 本机" };

    // 1) 预检（与前端 preflight op 同一实现：重名/权限/剩余空间/目标盘格式）
    //    lease 守卫保证长传输期间连接不被空闲回收
    let lease = ctx.registry.acquire_sftp(&host_id, ctx.client).await?;
    let sftp = lease.sftp.clone();
    let ctrl = SftpController::from_shared(sftp.clone());
    let (src, dst) = if upload {
        (local_path.as_str(), remote_path.as_str())
    } else {
        (remote_path.as_str(), local_path.as_str())
    };
    let report = crate::session::build_preflight_report(
        &ctrl,
        if upload { "upload" } else { "download" },
        src,
        dst,
        None,
    )
    .await
    .map_err(|e| ToolError::new(ERR_SFTP_FAILED, format!("传输预检失败：{e}")))?;
    if let Ok(v) = serde_json::to_value(&report) {
        detail.insert("preflight".into(), v);
    }

    // 2) 阻断项：权限/空间/目标为目录/不存在 → 结构化错误（AI 向用户转述原因）
    if !report.blockers.is_empty() {
        let reasons: Vec<&str> = report.blockers.iter().map(|b| b.message.as_str()).collect();
        return Err(ToolError::new(
            ERR_POLICY_DENIED,
            format!("传输未开始（预检阻断）：{}", reasons.join("；")),
        ));
    }
    if report.size_b > TRANSFER_MAX_BYTES {
        return Err(ToolError::new(
            ERR_POLICY_DENIED,
            format!("文件 {} 字节超过 AI 传输上限（4GB），请改用工作台文件面板手动传输", report.size_b),
        ));
    }

    // 3) 需对话内确认：目标重名（未指定 overwrite）/ ≥1GB（未指定 force）
    let target_exists = if upload { report.remote_exists } else { report.local_exists };
    if target_exists && !overwrite {
        return Ok(serde_json::json!({
            "needConfirm": true,
            "reason": "target_exists",
            "message": format!("目标{}已存在同名文件，请确认：回复『覆盖』（重传 overwrite=true）、改用建议路径，或取消",
                if upload { "远端" } else { "本地" }),
            "suggestedPath": report.suggested_path,
        }));
    }
    if report.size_b >= LARGE_FILE_BYTES && !force {
        return Ok(serde_json::json!({
            "needConfirm": true,
            "reason": "large_file",
            "message": format!("传输量较大（{} 字节），耗时可能较长；确认继续请带 force=true 重试", report.size_b),
            "bytes": report.size_b,
        }));
    }

    // 4) 用户批准（描述含覆盖决定）
    let describe = format!(
        "{direction_label}\n主机 {}\n{} → {}（{} 字节{}）",
        host_label_of(ctx.stores, &host_id),
        src,
        dst,
        report.size_b,
        if overwrite { "，目标存在时覆盖" } else { "" },
    );
    guard_write(ctx, approver, &describe).await?;

    // 5) 复用原生引擎（断点续传 + 句柄确认关闭；AI 无 WS 进度出口 → noop 状态）
    let state = Arc::new(crate::sftp::XferState::noop());
    let result = if upload {
        crate::sftp::xfer_upload_one(
            sftp.clone(),
            std::path::PathBuf::from(&local_path),
            remote_path.clone(),
            state.clone(),
            String::new(),
            overwrite,
        )
        .await
    } else {
        // 本地目标父目录缺失时自动创建（与工作台保存体验一致）
        if let Some(parent) = std::path::Path::new(&local_path).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
        }
        crate::sftp::xfer_download_one(
            sftp.clone(),
            remote_path.clone(),
            std::path::PathBuf::from(&local_path),
            state.clone(),
            String::new(),
            overwrite,
        )
        .await
    };
    match result {
        Ok(new_bytes) => {
            detail.insert("bytes".into(), serde_json::json!(report.size_b));
            Ok(serde_json::json!({
                "direction": direction_label,
                "localPath": local_path,
                "remotePath": remote_path,
                "bytes": report.size_b,
                "transferredB": new_bytes,
                "resumed": new_bytes < report.size_b,
            }))
        }
        Err(e) => {
            // 网络类错误：连接大概率已坏，驱逐注册表条目（下次操作自动重建）
            if crate::sftp::classify_sftp_error(&e).as_deref() == Some("network") {
                ctx.registry.evict(&host_id, "connection-error");
            }
            Err(ToolError::new(ERR_SFTP_FAILED, format!("传输中断：{e}")))
        }
    }
}

// ── 本地（Windows）命令安全分级 ───────────────────────────────────────────

fn local_deny_patterns() -> &'static Vec<Regex> {
    static PATTERNS: std::sync::OnceLock<Vec<Regex>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // 格式化 / 磁盘与引导（注意排除 Format-Table 等格式化输出 cmdlet）
            r"(?i)\bformat-volume\b|\bformat\s+[a-z]:|\bdiskpart\b|\bbcdedit\b|\bvssadmin\b",
            // 关机重启 / 危险系统程序
            r"(?i)\b(stop|restart)-computer\b|\bshutdown\b|\brundll32\b|\bregsvr32\b",
            // 递归删除盘根/系统目录
            r"(?i)remove-item\s+[^|;]*-recurse[^|;]*\s+[a-z]:\\?\s*$|remove-item\s+[^|;]*-recurse[^|;]*\s+[a-z]:\\(windows|users|program)",
            r"(?i)\b(rd|rmdir)\s+/s\b|\bdel\s+[^|;]*/[sq]\b|\brm\s+[^|;]*-r[a-z]*f",
            // 注册表与系统权限
            r"(?i)\breg\s+(delete|add)\s+[^|;]*hk(lm|cr|u)\\",
            r"(?i)\bcipher\s+/w\b|\btakeown\b|\bschtasks\s+/create\b",
            // 修改 SSH 自身凭据面
            r"(?i)\.ssh\\(authorized_keys|id_rsa|id_ed25519)",
        ]
        .iter()
        .map(|s| Regex::new(s).unwrap())
        .collect()
    })
}

fn local_allow_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        // 只读查询 + 打包/解包（产出新文件，不破坏既有数据）+ 构建类常用命令
        Regex::new(r"(?i)^\s*(?:get-|dir\b|ls\b|cat\b|type\b|pwd\b|whoami\b|hostname\b|echo\b|test-|select-|measure-|where\.exe|sort-|group-|compare-|compress-archive\b|expand-archive\b|tar\s|zip\s|unzip\b|7z\s|node\s+--?v|npm\s+(ls|list|view|run\s+build)|npx\s+--?v|git\s+(status|log|diff|branch)|systeminfo\b|tasklist\b|where\.exe)").unwrap()
    })
}

fn classify_local_command(command: &str) -> &'static str {
    if local_deny_patterns().iter().any(|re| re.is_match(command)) {
        return "deny";
    }
    if local_allow_re().is_match(command) {
        "allow"
    } else {
        "ask"
    }
}

// ── 本地命令执行（PowerShell，无窗口，UTF-8 输出，超时强杀）──────────────

struct LocalExecResult {
    code: i32,
    stdout: String,
    stderr: String,
    truncated: bool,
    timed_out: bool,
}

fn truncate_bytes(buf: &[u8]) -> (String, bool) {
    if buf.len() <= LOCAL_EXEC_OUTPUT_MAX {
        (String::from_utf8_lossy(buf).into_owned(), false)
    } else {
        // 回退到 UTF-8 字符边界（continuation byte 0b10xxxxxx 不是边界）
        let mut end = LOCAL_EXEC_OUTPUT_MAX;
        while end > 0 && buf[end] & 0xC0 == 0x80 {
            end -= 1;
        }
        (String::from_utf8_lossy(&buf[..end]).into_owned(), true)
    }
}

async fn read_pipe<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut truncated = false;
    if let Some(mut p) = pipe {
        let mut chunk = vec![0u8; TRANSFER_CHUNK];
        loop {
            match p.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() + n <= LOCAL_EXEC_OUTPUT_MAX * 2 {
                        buf.extend_from_slice(&chunk[..n]);
                    } else {
                        truncated = true;
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
    (buf, truncated)
}

async fn run_local_command(command: &str, timeout_ms: u64) -> Result<LocalExecResult, String> {
    use std::process::Stdio;

    let mut cmd = if cfg!(windows) {
        // 强制 UTF-8 输出，避免中文乱码
        let wrapped = format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {command}");
        let mut c = tokio::process::Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &wrapped]);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动本地命令失败: {e}"))?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let out_task = tokio::spawn(read_pipe(stdout_pipe));
    let err_task = tokio::spawn(read_pipe(stderr_pipe));

    match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
        Ok(Ok(status)) => {
            let out = out_task.await.map_err(|e| e.to_string())?;
            let err = err_task.await.map_err(|e| e.to_string())?;
            let (stdout, t1) = truncate_bytes(&out.0);
            let (stderr, t2) = truncate_bytes(&err.0);
            Ok(LocalExecResult {
                code: status.code().unwrap_or(-1),
                stdout,
                stderr,
                truncated: t1 || t2,
                timed_out: false,
            })
        }
        Ok(Err(e)) => Err(format!("本地命令执行失败: {e}")),
        Err(_) => {
            let _ = child.start_kill();
            Ok(LocalExecResult {
                code: -1,
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: true,
            })
        }
    }
}

// ── 工具定义导出（MCP tools/list 与 LLM function calling 双格式）──────────

/// 工具静态描述（单一事实来源，两种导出格式共用）
struct ToolSpec {
    name: &'static str,
    title: &'static str,
    desc: &'static str,
    params: serde_json::Value, // { "参数名": {"type","description"} }
    required: &'static [&'static str],
    read_only: bool,
    destructive: bool,
}

fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "list_hosts", title: "列出主机", read_only: true, destructive: false,
            desc: "列出 BitGalaxy Shell 主机档案（脱敏：不含任何凭据）。返回 id、名称、地址、用户、标签与 hostKeyKnown（指纹是否已确认；false 的主机需先在工作台人工连接一次）。",
            params: serde_json::json!({}), required: &[],
        },
        ToolSpec {
            name: "probe_host", title: "探测主机连通性", read_only: true, destructive: false,
            desc: "对目标主机做 TCP 端口探测（不做 SSH 握手，不使用凭据），返回延迟或失败原因。",
            params: serde_json::json!({ "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" } }),
            required: &["hostId"],
        },
        ToolSpec {
            name: "get_metrics", title: "采集主机运行指标", read_only: true, destructive: false,
            desc: "采集一次主机运行指标（uptime、负载、内存、磁盘、top 进程）。适用于 Linux 目标；主机需已在已知指纹列表。",
            params: serde_json::json!({ "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" } }),
            required: &["hostId"],
        },
        ToolSpec {
            name: "list_dir", title: "列远程目录", read_only: true, destructive: false,
            desc: "SFTP 列出远程目录内容（名称/类型/大小/mtime）。path 支持相对路径（相对登录用户家目录）。",
            params: serde_json::json!({
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
                "path": { "type": "string", "description": "远程目录路径" } }),
            required: &["hostId", "path"],
        },
        ToolSpec {
            name: "read_file", title: "读远程文本文件", read_only: true, destructive: false,
            desc: "SFTP 读取远程文本文件（UTF-8）。默认最多读取 256KB（受策略钳制），超限返回截断内容并标注 truncated 与 sizeB。大文件用 sftp_download_file。",
            params: serde_json::json!({
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
                "path": { "type": "string", "description": "远程文件路径" },
                "offset": { "type": "integer", "minimum": 0, "description": "起始字节偏移（默认 0）" },
                "maxBytes": { "type": "integer", "minimum": 1, "description": "本次最多读取字节数（受策略上限钳制）" } }),
            required: &["hostId", "path"],
        },
        ToolSpec {
            name: "exec_command", title: "执行远程命令", read_only: false, destructive: false,
            desc: "在目标主机上执行一次性非交互命令（ssh exec 通道），返回 stdout/stderr/退出码。命令经安全策略分级：内置黑名单直接拒绝；只读/白名单直接放行；其余需用户审批。超时默认 30s（可传 timeoutMs，受策略上限钳制），输出超 64KB 截断。",
            params: serde_json::json!({
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
                "command": { "type": "string", "description": "要执行的命令（非交互；交互式命令会因 stdin 关闭而失败）" },
                "timeoutMs": { "type": "integer", "minimum": 1000, "description": "超时毫秒数（受策略上限钳制）" } }),
            required: &["hostId", "command"],
        },
        ToolSpec {
            name: "write_file", title: "写远程文本文件", read_only: false, destructive: false,
            desc: "SFTP 写入远程文本文件（UTF-8，最大 1MB）。mode=overwrite 覆盖 / append 追加。受写策略与审批约束。",
            params: serde_json::json!({
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
                "path": { "type": "string", "description": "远程文件路径" },
                "content": { "type": "string", "description": "写入内容" },
                "mode": { "type": "string", "enum": ["overwrite", "append"], "description": "默认 overwrite" } }),
            required: &["hostId", "path", "content"],
        },
        ToolSpec {
            name: "make_dir", title: "建远程目录", read_only: false, destructive: false,
            desc: "SFTP 创建远程目录（单级，等价 mkdir）。路径不支持 ~ 波浪号展开（会当作字面目录名），相对路径相对家目录；不确定时先 list_dir 确认 resolvedPath。受写策略与审批约束。",
            params: serde_json::json!({
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
                "path": { "type": "string", "description": "远程目录路径（不要带 ~）" } }),
            required: &["hostId", "path"],
        },
        ToolSpec {
            name: "rename_path", title: "改名/移动远程路径", read_only: false, destructive: false,
            desc: "SFTP 改名或移动远程文件/目录。受写策略与审批约束。",
            params: serde_json::json!({
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
                "from": { "type": "string", "description": "原路径" },
                "to": { "type": "string", "description": "目标路径" } }),
            required: &["hostId", "from", "to"],
        },
        ToolSpec {
            name: "delete_path", title: "删除远程路径", read_only: false, destructive: true,
            desc: "SFTP 删除远程文件；recursive=true 删除整个目录树（危险）。受写策略与审批约束；递归删除根目录/家目录被硬性拒绝。",
            params: serde_json::json!({
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
                "path": { "type": "string", "description": "远程路径" },
                "recursive": { "type": "boolean", "description": "递归删除目录树" } }),
            required: &["hostId", "path"],
        },
        ToolSpec {
            name: "local_exec", title: "执行本机命令", read_only: false, destructive: false,
            desc: "在用户本机（Windows）执行 PowerShell 命令，返回 stdout/stderr/退出码。适合打包（Compress-Archive、tar）、构建、查询本机信息。危险命令会被拒绝；未识别命令需用户批准。超时默认 120s（timeoutSec 上限 600）。",
            params: serde_json::json!({
                "command": { "type": "string", "description": "PowerShell 命令（非交互）" },
                "timeoutSec": { "type": "number", "description": "超时秒数，默认 120，上限 600" } }),
            required: &["command"],
        },
        ToolSpec {
            name: "local_read_file", title: "读本机文件", read_only: true, destructive: false,
            desc: "读取本机文本文件（UTF-8，单次最多 256KB，支持 offset 分段）。只读自动放行；私钥/凭据类路径会被拒绝。",
            params: serde_json::json!({
                "path": { "type": "string", "description": "本机文件绝对路径" },
                "offset": { "type": "integer", "description": "起始字节偏移（默认 0）" },
                "maxBytes": { "type": "integer", "description": "本次最多读取字节数（上限 262144）" } }),
            required: &["path"],
        },
        ToolSpec {
            name: "local_write_file", title: "写本机文件", read_only: false, destructive: false,
            desc: "写本机文本文件（UTF-8，最大 1MB；mode=overwrite 覆盖/append 追加）。受写策略与审批约束；系统目录被拒绝。",
            params: serde_json::json!({
                "path": { "type": "string", "description": "本机文件绝对路径" },
                "content": { "type": "string", "description": "写入内容" },
                "mode": { "type": "string", "enum": ["overwrite", "append"], "description": "默认 overwrite" } }),
            required: &["path", "content"],
        },
        ToolSpec {
            name: "sftp_upload_file", title: "上传文件到主机", read_only: false, destructive: true,
            desc: "把本机文件流式上传到远程主机（不经对话内容，单文件最大 4GiB）。自动预检重名/权限/剩余空间/目标盘格式；目标已存在且 overwrite=false 时返回 needConfirm(target_exists, suggestedPath)，用户决定覆盖或换名后带 overwrite=true 或新路径重试；文件 ≥1GB 且 force=false 时返回 needConfirm(large_file)。上传前需用户批准。适合上传压缩包、构建产物。",
            params: serde_json::json!({
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
                "localPath": { "type": "string", "description": "本机文件绝对路径" },
                "remotePath": { "type": "string", "description": "远程目标路径" },
                "overwrite": { "type": "boolean", "description": "目标已存在时是否覆盖（默认 false，重名会返回 needConfirm）" },
                "force": { "type": "boolean", "description": "大文件（≥1GB）用户已确认，跳过确认直接传输（默认 false）" } }),
            required: &["hostId", "localPath", "remotePath"],
        },
        ToolSpec {
            name: "sftp_download_file", title: "从主机下载文件", read_only: false, destructive: true,
            desc: "把远程主机文件流式下载到本机（不经对话内容，单文件最大 4GiB）。自动预检重名/权限/剩余空间/本地盘格式（FAT32 超 4GB 直接拒绝）；目标已存在且 overwrite=false 时返回 needConfirm(target_exists, suggestedPath)；文件 ≥1GB 且 force=false 时返回 needConfirm(large_file)。下载前需用户批准。适合拉取日志、备份、构建产物。",
            params: serde_json::json!({
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
                "remotePath": { "type": "string", "description": "远程文件路径" },
                "localPath": { "type": "string", "description": "本机目标绝对路径" },
                "overwrite": { "type": "boolean", "description": "目标已存在时是否覆盖（默认 false，重名会返回 needConfirm）" },
                "force": { "type": "boolean", "description": "大文件（≥1GB）用户已确认，跳过确认直接传输（默认 false）" } }),
            required: &["hostId", "remotePath", "localPath"],
        },
    ]
}

/// MCP `tools/list` 格式
pub fn mcp_tool_defs() -> serde_json::Value {
    serde_json::json!(tool_specs()
        .into_iter()
        .map(|t| {
            let mut annotations = serde_json::Map::new();
            if t.read_only {
                annotations.insert("readOnlyHint".into(), serde_json::json!(true));
            }
            if t.destructive {
                annotations.insert("destructiveHint".into(), serde_json::json!(true));
            }
            serde_json::json!({
                "name": t.name,
                "title": t.title,
                "description": t.desc,
                "inputSchema": {
                    "type": "object",
                    "properties": t.params,
                    "required": t.required,
                },
                "annotations": annotations,
            })
        })
        .collect::<Vec<_>>())
}

/// OpenAI 兼容 function calling 格式
pub fn llm_tools_json() -> serde_json::Value {
    serde_json::json!(tool_specs()
        .into_iter()
        .map(|t| {
            serde_json::json!({ "type": "function", "function": {
                "name": t.name,
                "description": t.desc,
                "parameters": {
                    "type": "object",
                    "properties": t.params,
                    "required": t.required,
                } } })
        })
        .collect::<Vec<_>>())
}
