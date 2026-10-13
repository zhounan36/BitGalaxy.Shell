//! Agent 接入面④：MCP server（stdio，docs/ai-design.md §2）。
//! 由 AI 客户端作为子进程拉起（bgshell-desktop --mcp-stdio）；
//! stdout 专用于 MCP 协议（newline-delimited JSON-RPC），诊断只走 stderr。
//! 工具执行、策略、审计统一在 toolcore；本文件只负责协议层与 elicitation 审批。
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

use crate::policy::AiPolicy;
use crate::pool::ConnRegistry;
use crate::stores::Stores;
use crate::toolcore::{self, ApproveFuture, Approver, ToolCtx, ToolError};

const ELICIT_TIMEOUT_MS: u64 = 120_000;

// ── MCP stdio 主循环 ─────────────────────────────────────────────────────

struct McpState {
    stores: Stores,
    policy: AiPolicy,
    registry: ConnRegistry,
    client_label: String,
    /// 客户端是否声明支持 elicitation（initialize 后更新）
    elicit_supported: Arc<AtomicBool>,
    next_id: Arc<AtomicI64>,
    pending: Arc<StdMutex<HashMap<i64, oneshot::Sender<serde_json::Value>>>>,
    stdout: tokio::io::Stdout,
}

/// elicitation 审批实现（server→client 审批请求；不支持/超时/拒绝 均 fail-closed）
struct McpApprover {
    elicit_supported: Arc<AtomicBool>,
    next_id: Arc<AtomicI64>,
    pending: Arc<StdMutex<HashMap<i64, oneshot::Sender<serde_json::Value>>>>,
}

impl Approver for McpApprover {
    fn approve(&self, describe: &str) -> ApproveFuture {
        let describe = describe.to_string();
        let elicit_supported = self.elicit_supported.clone();
        let next_id = self.next_id.clone();
        let pending = self.pending.clone();
        Box::pin(async move {
            if !elicit_supported.load(Ordering::Relaxed) {
                return Err(ToolError::new(
                    toolcore::ERR_ELICIT_UNSUPPORTED,
                    "MCP 客户端未声明 elicitation 能力，无法请求审批（fail-closed）",
                ));
            }
            let req_id = next_id.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = oneshot::channel::<serde_json::Value>();
            pending.lock().unwrap().insert(req_id, tx);
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "elicitation/create",
                "params": {
                    "message": describe,
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
            let mut line = serde_json::to_string(&request).unwrap_or_default();
            line.push('\n');
            let mut stdout = tokio::io::stdout();
            let _ = stdout.write_all(line.as_bytes()).await;
            let _ = stdout.flush().await;
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
        })
    }
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
    // MCP stdio 为独立进程：自建注册表实例（与桌面进程代码同源、池各自独立）
    let registry = ConnRegistry::new(stores.clone(), policy.limits.idle_close_sec);

    let mut state = McpState {
        stores: stores.clone(),
        policy,
        registry,
        client_label: "unknown".into(),
        elicit_supported: Arc::new(AtomicBool::new(false)),
        next_id: Arc::new(AtomicI64::new(1)),
        pending: Arc::new(StdMutex::new(HashMap::new())),
        stdout: tokio::io::stdout(),
    };

    state.stores.audit.append(
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

    // stdio 关闭（客户端退出）→ 关闭注册表全部连接并退出进程
    state.registry.close_all("shutdown");
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
            state
                .elicit_supported
                .store(params.pointer("/capabilities/elicitation").is_some(), Ordering::Relaxed);
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
            send_result(state, &id, serde_json::json!({ "tools": toolcore::mcp_tool_defs() })).await;
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

/// 工具调用 → toolcore 统一执行 → MCP content 包装
async fn call_tool(state: &mut McpState, name: &str, args: &serde_json::Value) -> serde_json::Value {
    let approver = McpApprover {
        elicit_supported: state.elicit_supported.clone(),
        next_id: state.next_id.clone(),
        pending: state.pending.clone(),
    };
    let ctx = ToolCtx {
        stores: &state.stores,
        policy: &state.policy,
        registry: &state.registry,
        client: &state.client_label,
    };
    let payload = toolcore::execute(&ctx, &approver, name, args).await;
    let ok = payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    serde_json::json!({
        "content": [ { "type": "text", "text": payload.to_string() } ],
        "isError": !ok,
    })
}
