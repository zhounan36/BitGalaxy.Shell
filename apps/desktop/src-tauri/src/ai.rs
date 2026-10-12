//! 桌宠 AI 对话：OpenAI 兼容接口（默认 DeepSeek）工具循环在本进程执行，工具复用 SSH 层。
//!   list_hosts / probe_host / list_dir（只读自动放行）
//!   exec_command（黑名单硬拒；白名单直放；其余经桌宠弹卡片人工批准，TOFU fail-closed）
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::protocol::{AiApprovalM, AiDeltaM, AiResultM, AiStatusM, ServerFrame};
use crate::session::probe_tcp;
use crate::ssh::{HostKeyAsk, HostKeyCtx, SshConnection, SshError, SshKind};
use crate::stores::Stores;
use crate::wire::Outbound;

const IDLE_CLOSE_MS: u64 = 60_000;
const MAX_HISTORY: usize = 40;
const MAX_TOOL_ROUNDS: usize = 8;
const APPROVAL_TIMEOUT_MS: u64 = 120_000;

// ── 配置（SQLite settings 优先，环境变量兜底）─────────────────────────────

pub struct AiConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

fn load_ai_config(stores: &Stores) -> Option<AiConfig> {
    let mut api_key = stores.settings.get_str("ai.apiKey");
    let mut base_url = stores.settings.get_str("ai.baseUrl");
    let mut model = stores.settings.get_str("ai.model");
    api_key = api_key.or_else(|| std::env::var("DEEPSEEK_API_KEY").ok().or_else(|| std::env::var("OPENAI_API_KEY").ok()));
    base_url = base_url.or_else(|| std::env::var("BGSH_AI_BASE_URL").ok());
    model = model.or_else(|| std::env::var("BGSH_AI_MODEL").ok());
    let api_key = api_key?;
    Some(AiConfig {
        api_key,
        base_url: base_url.unwrap_or_else(|| "https://api.deepseek.com".into()),
        model: model.unwrap_or_else(|| "deepseek-chat".into()),
    })
}

// ── 命令安全分级（黑名单硬拒 / 白名单直放 / 其余人工批准）─────────────────

fn deny_patterns() -> &'static Vec<Regex> {
    static PATTERNS: std::sync::OnceLock<Vec<Regex>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"(?i)rm\s+(-[a-z]*f[a-z]*\s+)*-(?:[a-z]*r[a-z]*f|[a-z]*f[a-z]*r)\b|rm\s+-rf?\s+/(?:\s|$)",
            r"(?i)\bmkfs(\.\w+)?\b|\bdd\s+if=",
            r"(?i)\b(shutdown|reboot|poweroff|halt|init\s+[06])\b",
            r":\(\)\s*\{\s*:\|:&\s*\}\s*;:|fork\s*\(\)\s*while",
            r"(?i)chmod\s+-R\s+777\s+/",
            r"(?i)>\s*/dev/(sd|nvme|hd)",
            r"(?i)\b(userdel|groupdel)\b.*\broot\b",
        ]
        .iter()
        .map(|s| Regex::new(s).unwrap())
        .collect()
    })
}

fn read_only_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*(?:sudo\s+)?(ls|ll|cat|head|tail|pwd|whoami|id|hostname|uname|uptime|date|echo|which|env|printenv|df|free|du|wc|stat|file|ps|top\s+-b|ip\s+(a|addr)|ifconfig|netstat|ss|lsof|grep|find|git\s+(status|log|diff)|systemctl\s+(status|list|is-active)|docker\s+(ps|images)|kubectl\s+get|cat\s+/etc/os-release|lsb_release)\b").unwrap()
    })
}

fn classify_command(command: &str) -> &'static str {
    if deny_patterns().iter().any(|re| re.is_match(command)) {
        return "deny";
    }
    if read_only_re().is_match(command) {
        "allow"
    } else {
        "ask"
    }
}

// ── SSH 连接池（按 hostId 复用，空闲 60s 关闭）────────────────────────────

struct PoolEntry {
    conn: Arc<SshConnection>,
    sftp: Option<Arc<russh_sftp::client::SftpSession>>,
    idle_task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct AiSshPool {
    stores: Stores,
    entries: Arc<StdMutex<HashMap<String, PoolEntry>>>,
    locks: Arc<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl AiSshPool {
    fn new(stores: Stores) -> AiSshPool {
        AiSshPool {
            stores,
            entries: Arc::new(StdMutex::new(HashMap::new())),
            locks: Arc::new(StdMutex::new(HashMap::new())),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn host_lock(&self, host_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap();
        locks
            .entry(host_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn evict(&self, host_id: &str) {
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
        }
    }

    fn close_all(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::Relaxed);
        let ids: Vec<String> = self.entries.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.evict(&id);
        }
    }

    async fn acquire(&self, host_id: &str) -> Result<Arc<SshConnection>, String> {
        if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("Agent 已停止".into());
        }
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
            .ok_or_else(|| format!("主机档案不存在: {host_id}"))?;
        let mismatch = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = Arc::new(HostKeyCtx {
            known: self.stores.known.clone(),
            host: host.input.host.clone(),
            port: host.input.port,
            ask: HostKeyAsk::FailClosed,
            mismatch: mismatch.clone(),
        });
        let conn = match crate::ssh::connect(&host, &SshKind::Exec, ctx).await {
            Ok(c) => Arc::new(c),
            Err(SshError::HostKeyRejected) => {
                return Err(format!(
                    "主机 {} 指纹未确认：请先在工作台人工连接一次后再交给 AI 使用",
                    host.input.name
                ))
            }
            Err(e) => return Err(e.message()),
        };
        self.insert(host_id, conn.clone(), None);
        Ok(conn)
    }

    /// sftp 子系统会话（缓存于连接池条目）
    async fn acquire_sftp(&self, host_id: &str) -> Result<Arc<russh_sftp::client::SftpSession>, String> {
        if let Some(entry) = self.entries.lock().unwrap().get(host_id) {
            if let Some(sftp) = entry.sftp.clone() {
                self.touch_idle(host_id);
                return Ok(sftp);
            }
        }
        let conn = self.acquire(host_id).await?;
        let sftp = Arc::new(conn.open_sftp().await?);
        if let Some(entry) = self.entries.lock().unwrap().get_mut(host_id) {
            entry.sftp = Some(sftp.clone());
        }
        self.touch_idle(host_id);
        Ok(sftp)
    }

    fn insert(&self, host_id: &str, conn: Arc<SshConnection>, sftp: Option<Arc<russh_sftp::client::SftpSession>>) {
        let pool = self.clone();
        let id = host_id.to_string();
        let idle_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(IDLE_CLOSE_MS)).await;
            pool.evict(&id);
        });
        self.entries.lock().unwrap().insert(
            host_id.to_string(),
            PoolEntry { conn, sftp, idle_task },
        );
    }

    fn touch_idle(&self, host_id: &str) {
        let pool = self.clone();
        let id = host_id.to_string();
        let idle_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(IDLE_CLOSE_MS)).await;
            pool.evict(&id);
        });
        if let Some(entry) = self.entries.lock().unwrap().get_mut(host_id) {
            entry.idle_task.abort();
            entry.idle_task = idle_task;
        }
    }
}

// ── LLM 消息与工具 ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

fn system_prompt() -> String {
    [
        "你是 BitGalaxy Shell 的桌宠助手「鲸鱼娘」，帮用户管理 SSH 主机。",
        "风格：中文、简短、活泼但专业；汇报命令结果时给出要点总结，不要大段贴原文（超 20 行只说关键部分）。",
        "可用工具：list_hosts 查主机；probe_host 测连通；exec_command 在主机上执行命令（危险命令需要用户在宠物弹窗批准）；list_dir 列远程目录。",
        "主机 id 必须先用 list_hosts 查询；用户说\"web 服务器\"这类称呼时按名称/标签匹配。",
        "执行失败时如实说明原因；不要编造输出。",
    ]
    .join("\n")
}

fn tools_json() -> serde_json::Value {
    serde_json::json!([
        { "type": "function", "function": {
            "name": "list_hosts",
            "description": "列出所有主机档案（脱敏，不含凭据）。返回 id、名称、地址、用户、标签与 hostKeyKnown（指纹是否已确认）。",
            "parameters": { "type": "object", "properties": {} } } },
        { "type": "function", "function": {
            "name": "probe_host",
            "description": "TCP 探测主机连通性（不做 SSH 握手），返回延迟或失败原因。",
            "parameters": { "type": "object", "properties": {
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" } },
                "required": ["hostId"] } } },
        { "type": "function", "function": {
            "name": "exec_command",
            "description": "在主机上执行一次性非交互命令，返回 stdout/stderr/退出码。危险命令会先向用户弹卡片请求批准，未批准则返回拒绝。",
            "parameters": { "type": "object", "properties": {
                "hostId": { "type": "string", "description": "主机 id（先用 list_hosts 查询）" },
                "command": { "type": "string", "description": "要执行的命令（非交互）" },
                "timeoutSec": { "type": "number", "description": "超时秒数，默认 30，上限 120" } },
                "required": ["hostId", "command"] } } },
        { "type": "function", "function": {
            "name": "list_dir",
            "description": "SFTP 列出远程目录内容（名称/类型/大小/修改时间）。",
            "parameters": { "type": "object", "properties": {
                "hostId": { "type": "string", "description": "主机 id" },
                "path": { "type": "string", "description": "远程目录路径，支持相对家目录" } },
                "required": ["hostId", "path"] } } },
    ])
}

// ── handler ──────────────────────────────────────────────────────────────

struct Shared {
    stores: Stores,
    out: Outbound,
    pool: AiSshPool,
    approvals: StdMutex<HashMap<String, oneshot::Sender<bool>>>,
    chats: StdMutex<HashMap<String, Vec<LlmMessage>>>,
    busy: StdMutex<HashSet<String>>,
}

#[derive(Clone)]
pub struct AiHandler {
    shared: Arc<Shared>,
}

impl AiHandler {
    pub fn new(stores: Stores, out: Outbound) -> AiHandler {
        AiHandler {
            shared: Arc::new(Shared {
                pool: AiSshPool::new(stores.clone()),
                stores,
                out,
                approvals: StdMutex::new(HashMap::new()),
                chats: StdMutex::new(HashMap::new()),
                busy: StdMutex::new(HashSet::new()),
            }),
        }
    }

    pub fn handle_ai(&self, m: serde_json::Value) {
        let op = m.get("op").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let chat_id = m.get("chatId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        match op.as_str() {
            "reset" => {
                self.shared.chats.lock().unwrap().remove(&chat_id);
            }
            "chat" => {
                let text = m.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                {
                    let mut busy = self.shared.busy.lock().unwrap();
                    if busy.contains(&chat_id) {
                        self.shared.out.send_frame(&ServerFrame::AiResult {
                            m: AiResultM {
                                chat_id: chat_id.clone(),
                                ok: false,
                                text: String::new(),
                                message: Some("上一轮还在执行中，稍等一下～".into()),
                            },
                        });
                        return;
                    }
                    busy.insert(chat_id.clone());
                }
                let this = self.clone();
                tokio::spawn(async move {
                    this.run_turn(&chat_id, &text).await;
                    this.shared.busy.lock().unwrap().remove(&chat_id);
                });
            }
            _ => {}
        }
    }

    pub fn handle_approve(&self, m: &AiApprovalM) {
        if let Some(tx) = self.shared.approvals.lock().unwrap().remove(&m.req_id) {
            let _ = tx.send(m.approved);
        }
    }

    pub fn dispose(&self) {
        let senders: Vec<oneshot::Sender<bool>> =
            self.shared.approvals.lock().unwrap().drain().map(|(_, v)| v).collect();
        for tx in senders {
            let _ = tx.send(false);
        }
        self.shared.pool.close_all();
    }

    fn status(&self, chat_id: &str, stage: &str, detail: Option<String>) {
        self.shared.out.send_frame(&ServerFrame::AiStatus {
            m: AiStatusM {
                chat_id: chat_id.to_string(),
                stage: stage.to_string(),
                detail,
            },
        });
    }

    /// 请求用户批准（桌宠弹卡片）；超时视为拒绝
    async fn ask_approval(&self, chat_id: &str, host: &str, command: &str) -> bool {
        let req_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<bool>();
        self.shared.approvals.lock().unwrap().insert(req_id.clone(), tx);
        self.shared.out.send_frame(&ServerFrame::AiApproval {
            m: AiApprovalM {
                chat_id: chat_id.to_string(),
                req_id,
                host: host.to_string(),
                command: command.to_string(),
            },
        });
        match tokio::time::timeout(Duration::from_millis(APPROVAL_TIMEOUT_MS), rx).await {
            Ok(Ok(approved)) => approved,
            _ => false,
        }
    }

    async fn run_tool(&self, chat_id: &str, name: &str, args: &serde_json::Value) -> serde_json::Value {
        let stores = &self.shared.stores;
        match name {
            "list_hosts" => {
                let hosts: Vec<serde_json::Value> = stores
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
                            "hostKeyKnown": stores.known.find(&h.input.host, h.input.port).is_some(),
                        })
                    })
                    .collect();
                serde_json::json!({ "ok": true, "hosts": hosts })
            }
            "probe_host" => {
                let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default();
                match stores.hosts.get(host_id) {
                    None => serde_json::json!({ "ok": false, "message": "主机档案不存在" }),
                    Some(host) => {
                        let r = probe_tcp(&host.input.host, host.input.port).await;
                        serde_json::json!({
                            "ok": r.ok,
                            "ms": r.ms,
                            "message": r.message,
                        })
                    }
                }
            }
            "exec_command" => self.tool_exec(chat_id, args).await,
            "list_dir" => self.tool_list_dir(args).await,
            _ => serde_json::json!({ "ok": false, "message": format!("未知工具: {name}") }),
        }
    }

    async fn tool_exec(&self, chat_id: &str, args: &serde_json::Value) -> serde_json::Value {
        let stores = &self.shared.stores;
        let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let Some(host) = stores.hosts.get(&host_id) else {
            return serde_json::json!({ "ok": false, "message": "主机档案不存在" });
        };
        if command.trim().is_empty() {
            return serde_json::json!({ "ok": false, "message": "命令为空" });
        }
        let cls = classify_command(&command);
        if cls == "deny" {
            stores.audit.append(
                "ai.tool",
                serde_json::json!({ "tool": "exec_command", "hostId": host_id, "command": command, "decision": "deny" }),
            );
            return serde_json::json!({ "ok": false, "message": "命令命中安全黑名单，已拒绝执行" });
        }
        let mut approved = cls == "allow";
        if !approved {
            self.status(chat_id, "awaiting-approval", Some(command.chars().take(120).collect()));
            approved = self
                .ask_approval(
                    chat_id,
                    &format!("{}（{}）", host.input.name, host_label(&host)),
                    &command,
                )
                .await;
        }
        stores.audit.append(
            "ai.tool",
            serde_json::json!({
                "tool": "exec_command", "hostId": host_id, "command": command,
                "decision": if approved { "allow" } else { "approval-declined" },
            }),
        );
        if !approved {
            return serde_json::json!({ "ok": false, "message": "用户未批准该命令，未执行" });
        }
        let timeout_sec = args.get("timeoutSec").and_then(|v| v.as_f64()).unwrap_or(30.0);
        let timeout_ms = (timeout_sec * 1000.0).clamp(1000.0, 120_000.0) as u64;
        match self.shared.pool.acquire(&host_id).await {
            Ok(conn) => match conn.exec(&command, timeout_ms, 64 * 1024).await {
                Ok(r) => serde_json::json!({
                    "ok": true,
                    "exitCode": r.code,
                    "stdout": r.stdout,
                    "stderr": r.stderr,
                    "truncated": r.truncated,
                    "timedOut": r.timed_out,
                }),
                Err(e) => {
                    self.shared.pool.evict(&host_id);
                    serde_json::json!({ "ok": false, "message": e })
                }
            },
            Err(e) => serde_json::json!({ "ok": false, "message": e }),
        }
    }

    async fn tool_list_dir(&self, args: &serde_json::Value) -> serde_json::Value {
        let host_id = args.get("hostId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".").to_string();
        match self.shared.pool.acquire_sftp(&host_id).await {
            Ok(sftp) => {
                use futures_util::stream::StreamExt;
                match sftp.read_dir(&path).await {
                    Ok(dir) => {
                        let mut entries: Vec<serde_json::Value> = Vec::new();
                        let mut dir = Box::pin(dir);
                        while let Some(entry) = dir.next().await {
                            let name = entry.file_name();
                            if name == "." || name == ".." {
                                continue;
                            }
                            let meta = entry.metadata();
                            entries.push(serde_json::json!({
                                "name": name,
                                "isDir": meta.is_dir(),
                                "sizeB": meta.size().unwrap_or(0),
                                "mtimeMs": meta.mtime().map(|t| t * 1000).unwrap_or(0),
                            }));
                        }
                        serde_json::json!({ "ok": true, "path": path, "entries": entries })
                    }
                    Err(e) => {
                        self.shared.pool.evict(&host_id);
                        serde_json::json!({ "ok": false, "message": e.to_string() })
                    }
                }
            }
            Err(e) => serde_json::json!({ "ok": false, "message": e }),
        }
    }

    async fn run_turn(&self, chat_id: &str, text: &str) {
        let out = &self.shared.out;
        let Some(cfg) = load_ai_config(&self.shared.stores) else {
            out.send_frame(&ServerFrame::AiResult {
                m: AiResultM {
                    chat_id: chat_id.to_string(),
                    ok: false,
                    text: String::new(),
                    message: Some(
                        "还没有配置 AI 服务：在工作台「设置 → AI 助手」填写 API Key 后重试（也可设置环境变量 DEEPSEEK_API_KEY）".into(),
                    ),
                },
            });
            return;
        };

        {
            let mut chats = self.shared.chats.lock().unwrap();
            let messages = chats.entry(chat_id.to_string()).or_default();
            messages.push(LlmMessage {
                role: "user".into(),
                content: Some(text.to_string()),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        self.status(chat_id, "thinking", None);

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                self.send_ai_error(chat_id, e.to_string());
                return;
            }
        };
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));

        for _round in 0..MAX_TOOL_ROUNDS {
            // 组装消息（首条 system）
            let mut req_messages: Vec<LlmMessage> = Vec::new();
            {
                let chats = self.shared.chats.lock().unwrap();
                if let Some(history) = chats.get(chat_id) {
                    let has_system = history.first().map(|m| m.role == "system").unwrap_or(false);
                    if !has_system {
                        req_messages.push(LlmMessage {
                            role: "system".into(),
                            content: Some(system_prompt()),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                    req_messages.extend(history.iter().cloned());
                }
            }

            let body = serde_json::json!({
                "model": cfg.model,
                "messages": req_messages,
                "tools": tools_json(),
                "tool_choice": "auto",
            });
            let resp = client
                .post(&url)
                .bearer_auth(&cfg.api_key)
                .json(&body)
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    self.send_ai_error(chat_id, e.to_string());
                    return;
                }
            };
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                self.send_ai_error(chat_id, format!("AI 服务返回 {status}: {}", truncate(&text, 300)));
                return;
            }
            let data: serde_json::Value = match resp.json().await {
                Ok(d) => d,
                Err(e) => {
                    self.send_ai_error(chat_id, format!("AI 服务返回解析失败: {e}"));
                    return;
                }
            };
            let msg_value = data
                .pointer("/choices/0/message")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let msg: LlmMessage = match serde_json::from_value(msg_value) {
                Ok(m) => m,
                Err(_) => {
                    self.send_ai_error(chat_id, "AI 服务返回为空");
                    return;
                }
            };

            let tool_calls = msg.tool_calls.clone();
            {
                let mut chats = self.shared.chats.lock().unwrap();
                if let Some(history) = chats.get_mut(chat_id) {
                    history.push(msg);
                }
            }

            if let Some(calls) = tool_calls.filter(|c| !c.is_empty()) {
                for tc in calls {
                    let args: serde_json::Value =
                        serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::json!({}));
                    self.status(
                        chat_id,
                        "tool",
                        Some(truncate(&format!("{} {}", tc.function.name, tc.function.arguments), 120)),
                    );
                    let result = self.run_tool(chat_id, &tc.function.name, &args).await;
                    let mut content = result.to_string();
                    if content.len() > 16_000 {
                        content.truncate(16_000);
                    }
                    let mut chats = self.shared.chats.lock().unwrap();
                    if let Some(history) = chats.get_mut(chat_id) {
                        history.push(LlmMessage {
                            role: "tool".into(),
                            content: Some(content),
                            tool_calls: None,
                            tool_call_id: Some(tc.id.clone()),
                        });
                    }
                }
                continue;
            }

            // 最终文本：分片下推模拟流式
            let full = msg.content.clone().unwrap_or_default();
            let mut offset = 0;
            while offset < full.len() {
                let end = (offset + 160).min(full.len());
                // 避免切断 UTF-8 字符边界
                let mut safe_end = end;
                while safe_end > offset && !full.is_char_boundary(safe_end) {
                    safe_end -= 1;
                }
                out.send_frame(&ServerFrame::AiDelta {
                    m: AiDeltaM {
                        chat_id: chat_id.to_string(),
                        delta: full[offset..safe_end].to_string(),
                    },
                });
                offset = safe_end;
            }
            out.send_frame(&ServerFrame::AiResult {
                m: AiResultM {
                    chat_id: chat_id.to_string(),
                    ok: true,
                    text: full,
                    message: None,
                },
            });
            let mut chats = self.shared.chats.lock().unwrap();
            if let Some(history) = chats.get_mut(chat_id) {
                if history.len() > MAX_HISTORY {
                    let excess = history.len() - MAX_HISTORY;
                    history.drain(..excess);
                }
            }
            return;
        }
        self.send_ai_error(chat_id, "工具循环轮次超限，任务太复杂了，试试拆小一点");
    }

    fn send_ai_error(&self, chat_id: &str, message: String) {
        self.shared.out.send_frame(&ServerFrame::AiResult {
            m: AiResultM {
                chat_id: chat_id.to_string(),
                ok: false,
                text: String::new(),
                message: Some(message),
            },
        });
    }
}

fn host_label(host: &crate::protocol::HostRecord) -> String {
    format!("{}@{}:{}", host.input.username, host.input.host, host.input.port)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}
