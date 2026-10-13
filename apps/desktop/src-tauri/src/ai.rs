//! 桌宠 AI 对话：OpenAI 兼容接口（默认 DeepSeek）。
//! 工具循环在本进程执行；工具实现、策略、审计统一在 toolcore，
//! 本文件只负责 LLM 对话循环与桌宠弹卡片审批（PetApprover）。
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::policy::AiPolicy;
use crate::pool::ConnRegistry;
use crate::protocol::{AiApprovalM, AiApproveM, AiDeltaM, AiResultM, AiStatusM, AiThinkingM, ServerFrame};
use crate::stores::Stores;
use crate::toolcore::{self, ApproveFuture, Approver, ToolCtx};
use crate::wire::Outbound;

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

// ── LLM 消息 ──────────────────────────────────────────────────────────────

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
        "你是 BitGalaxy Shell 的桌宠助手「鲸鱼娘」，帮用户管理 SSH 主机，也能操作用户本机（Windows）。",
        "风格：中文、简短、活泼但专业；汇报命令结果时给出要点总结，不要大段贴原文（超 20 行只说关键部分）。",
        "远程工具：list_hosts 查主机；probe_host 测连通；get_metrics 采指标；exec_command 在主机上执行命令；list_dir 列远程目录；read_file 读远程小文件；write_file 写远程文件；sftp_download_file 下载远程文件到本机。",
        "本地工具：local_exec 在本机执行 PowerShell（打包用 Compress-Archive 或 tar）；local_read_file/local_write_file 读写本机文件；sftp_upload_file 把本机文件上传到主机。",
        "典型流程：用户说「打包 X 上传到服务器」→ local_exec 打包 → sftp_upload_file 上传 →（可选）exec_command 解压/运行。",
        "安全：危险命令会被黑名单直接拒绝；写文件、上传下载及未识别命令会弹卡片请求用户批准，被拒时如实告知用户。文件大小超限时建议分卷或改用压缩。",
        "主机 id 必须先用 list_hosts 查询；用户说\"web 服务器\"这类称呼时按名称/标签匹配。路径不存在时先列目录确认。",
        "执行失败时如实说明原因；不要编造输出。",
    ]
    .join("\n")
}

// ── 桌宠审批实现（弹卡片；超时视为拒绝，fail-closed）──────────────────────

struct PetApprover {
    chat_id: String,
    out: Outbound,
    approvals: Arc<StdMutex<HashMap<String, oneshot::Sender<bool>>>>,
}

impl Approver for PetApprover {
    fn approve(&self, describe: &str) -> ApproveFuture {
        let chat_id = self.chat_id.clone();
        let out = self.out.clone();
        let approvals = self.approvals.clone();
        let describe = describe.to_string();
        Box::pin(async move {
            let req_id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel::<bool>();
            approvals.lock().unwrap().insert(req_id.clone(), tx);
            out.send_frame(&ServerFrame::AiStatus {
                m: AiStatusM {
                    chat_id: chat_id.clone(),
                    stage: "awaiting-approval".into(),
                    detail: Some(describe.chars().take(120).collect()),
                },
            });
            out.send_frame(&ServerFrame::AiApproval {
                m: AiApprovalM {
                    chat_id,
                    req_id,
                    host: "操作审批".into(),
                    command: describe,
                },
            });
            match tokio::time::timeout(Duration::from_millis(APPROVAL_TIMEOUT_MS), rx).await {
                Ok(Ok(approved)) => Ok(approved),
                _ => Ok(false),
            }
        })
    }
}

// ── handler ──────────────────────────────────────────────────────────────

struct Shared {
    stores: Stores,
    policy: AiPolicy,
    registry: Arc<ConnRegistry>,
    out: Outbound,
    approvals: Arc<StdMutex<HashMap<String, oneshot::Sender<bool>>>>,
    chats: StdMutex<HashMap<String, Vec<LlmMessage>>>,
    busy: StdMutex<HashSet<String>>,
}

#[derive(Clone)]
pub struct AiHandler {
    shared: Arc<Shared>,
}

impl AiHandler {
    pub fn new(
        stores: Stores,
        out: Outbound,
        policy: AiPolicy,
        registry: Arc<ConnRegistry>,
    ) -> AiHandler {
        AiHandler {
            shared: Arc::new(Shared {
                stores,
                policy,
                registry,
                out,
                approvals: Arc::new(StdMutex::new(HashMap::new())),
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

    pub fn handle_approve(&self, m: &AiApproveM) {
        if let Some(tx) = self.shared.approvals.lock().unwrap().remove(&m.req_id) {
            let _ = tx.send(m.approved);
        }
    }

    pub fn dispose(&self) {
        // WS 断开只撤审批；连接归注册表管（工作台会话由 close_all 各自 release，
        // AI 临时占用随工具执行结束自动释放，无引用后按空闲超时回收）
        let senders: Vec<oneshot::Sender<bool>> =
            self.shared.approvals.lock().unwrap().drain().map(|(_, v)| v).collect();
        for tx in senders {
            let _ = tx.send(false);
        }
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

    /// 分片下推模拟流式（thinking 块或正文），避免切断 UTF-8 字符边界
    fn push_stream(&self, chat_id: &str, full: &str, thinking: bool) {
        let mut offset = 0;
        while offset < full.len() {
            let end = (offset + 160).min(full.len());
            let mut safe_end = end;
            while safe_end > offset && !full.is_char_boundary(safe_end) {
                safe_end -= 1;
            }
            let frame = if thinking {
                ServerFrame::AiThinking {
                    m: AiThinkingM {
                        chat_id: chat_id.to_string(),
                        delta: full[offset..safe_end].to_string(),
                    },
                }
            } else {
                ServerFrame::AiDelta {
                    m: AiDeltaM {
                        chat_id: chat_id.to_string(),
                        delta: full[offset..safe_end].to_string(),
                    },
                }
            };
            self.shared.out.send_frame(&frame);
            offset = safe_end;
        }
    }

    /// 工具调用 → toolcore 统一执行（策略 + 审批 + 审计一致）
    async fn run_tool(&self, chat_id: &str, name: &str, args: &serde_json::Value) -> serde_json::Value {
        let approver = PetApprover {
            chat_id: chat_id.to_string(),
            out: self.shared.out.clone(),
            approvals: self.shared.approvals.clone(),
        };
        let ctx = ToolCtx {
            stores: &self.shared.stores,
            policy: &self.shared.policy,
            registry: self.shared.registry.as_ref(),
            client: "pet",
        };
        toolcore::execute(&ctx, &approver, name, args).await
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
                "tools": toolcore::llm_tools_json(),
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
            // 思考内容（DeepSeek reasoning_content / OpenAI 风格 reasoning）：
            // 只下推前端展示，不进历史（DeepSeek 要求历史消息不携带该字段）
            let reasoning = msg_value
                .get("reasoning_content")
                .or_else(|| msg_value.get("reasoning"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            if let Some(think) = reasoning.as_deref() {
                self.push_stream(chat_id, think, true);
            }
            let msg: LlmMessage = match serde_json::from_value(msg_value) {
                Ok(m) => m,
                Err(_) => {
                    self.send_ai_error(chat_id, "AI 服务返回为空");
                    return;
                }
            };

            let tool_calls = msg.tool_calls.clone();
            let final_content = msg.content.clone();
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
            let full = final_content.unwrap_or_default();
            self.push_stream(chat_id, &full, false);
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

    fn send_ai_error(&self, chat_id: &str, message: impl Into<String>) {
        self.shared.out.send_frame(&ServerFrame::AiResult {
            m: AiResultM {
                chat_id: chat_id.to_string(),
                ok: false,
                text: String::new(),
                message: Some(message.into()),
            },
        });
    }
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
