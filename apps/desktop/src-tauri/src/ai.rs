//! 桌宠 AI 对话：OpenAI 兼容接口（默认 DeepSeek）。
//! 工具循环在本进程执行；工具实现、策略、审计统一在 toolcore，
//! 本文件只负责 LLM 对话循环与桌宠弹卡片审批（PetApprover）。
//! AiHandler 为进程级单例（跨 WS 连接共享）：审批回包 / 终止请求在
//! 断线重连后的新连接上依然能路由到进行中的对话任务。
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::policy::AiPolicy;
use crate::pool::ConnRegistry;
use crate::protocol::{AiApprovalM, AiApproveM, AiDeltaM, AiResultM, AiStatusM, AiThinkingM, AiToolM, ServerFrame};
use crate::stores::Stores;
use crate::toolcore::{self, ApproveFuture, Approver, ToolCtx};
use crate::wire::OutHub;

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
        "主机 id 必须先用 list_hosts 查询；用户说\"web 服务器\"这类称呼时按名称/标签匹配。",
        "路径规则：SFTP 工具（list_dir/read_file/write_file/make_dir/rename_path/delete_path）不支持 ~ 波浪号展开（会被当作字面目录名）；相对路径相对登录用户家目录。不确定实际位置时先用 list_dir 查看 resolvedPath。",
        "权限规则：远程 exec 无终端（无 TTY），sudo 需要密码会直接失败，禁止执行 sudo 命令；遇到 Permission denied / 权限不足时如实报告原因，建议用户改到家目录等有权限的位置、在工作台终端手动 sudo，或配置 sudoers NOPASSWD 后再试。",
        "执行失败时如实说明原因（退出码 / stderr 要点）；不要编造输出，不要把失败说成被拒绝。",
    ]
    .join("\n")
}

// ── 桌宠审批实现（弹卡片；超时视为拒绝，fail-closed）──────────────────────

/// 待审批项：记录归属会话，便于「终止对话」时精确撤下对应卡片
struct PendingApproval {
    chat_id: String,
    tx: oneshot::Sender<bool>,
}

struct PetApprover {
    chat_id: String,
    out: OutHub,
    approvals: Arc<StdMutex<HashMap<String, PendingApproval>>>,
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
            approvals.lock().unwrap().insert(req_id.clone(), PendingApproval { chat_id: chat_id.clone(), tx });
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
                    req_id: req_id.clone(),
                    host: "操作审批".into(),
                    command: describe,
                },
            });
            let approved = match tokio::time::timeout(Duration::from_millis(APPROVAL_TIMEOUT_MS), rx).await {
                Ok(Ok(approved)) => approved,
                // 超时=拒绝（fail-closed）；无论结果如何都清理登记，避免 map 泄漏
                _ => false,
            };
            approvals.lock().unwrap().remove(&req_id);
            Ok(approved)
        })
    }
}

// ── handler ──────────────────────────────────────────────────────────────

struct Shared {
    stores: Stores,
    policy: AiPolicy,
    registry: Arc<ConnRegistry>,
    /// 广播出站：AI 帧发到所有活跃连接（断线重连后的新连接也能收到）
    out: OutHub,
    approvals: Arc<StdMutex<HashMap<String, PendingApproval>>>,
    chats: StdMutex<HashMap<String, Vec<LlmMessage>>>,
    busy: StdMutex<HashSet<String>>,
    /// 运行中的对话轮任务（chat_id → JoinHandle），供「终止」abort
    turns: StdMutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// 执行中的工具调用（toolCallId → 取消通道），供「终止单条命令」
    running_tools: StdMutex<HashMap<String, RunningTool>>,
}

/// 执行中工具的取消通道（记录归属会话，便于整轮终止时批量清理）
struct RunningTool {
    chat_id: String,
    tx: oneshot::Sender<()>,
}

/// busy 占用守卫：任务结束 / 被 abort / panic 都会 Drop，杜绝 busy 永久泄漏
struct BusyGuard {
    shared: Arc<Shared>,
    chat_id: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.shared.busy.lock().unwrap().remove(&self.chat_id);
    }
}

#[derive(Clone)]
pub struct AiHandler {
    shared: Arc<Shared>,
}

impl AiHandler {
    pub fn new(
        stores: Stores,
        out: OutHub,
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
                turns: StdMutex::new(HashMap::new()),
                running_tools: StdMutex::new(HashMap::new()),
            }),
        }
    }

    /// 新 WS 连接注册：AI 帧开始向该连接广播
    pub fn register_conn(&self, out: crate::wire::Outbound) {
        self.shared.out.register(out);
    }

    pub fn handle_ai(&self, m: serde_json::Value) {
        let op = m.get("op").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let chat_id = m.get("chatId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        match op.as_str() {
            "reset" => {
                self.cancel_turn(&chat_id);
                self.shared.chats.lock().unwrap().remove(&chat_id);
            }
            "cancel" => {
                self.cancel_turn(&chat_id);
            }
            "cancel-tool" => {
                let tool_call_id =
                    m.get("toolCallId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                self.cancel_tool(&chat_id, &tool_call_id);
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
                                message: Some("上一轮还在执行中，稍等一下～（也可以点「停止」终止它）".into()),
                            },
                        });
                        return;
                    }
                    busy.insert(chat_id.clone());
                }
                let this = self.clone();
                let chat = chat_id.clone();
                let handle = tokio::spawn(async move {
                    let _guard = BusyGuard { shared: this.shared.clone(), chat_id: chat.clone() };
                    this.run_turn(&chat, &text).await;
                    this.shared.turns.lock().unwrap().remove(&chat);
                });
                self.shared.turns.lock().unwrap().insert(chat_id, handle);
            }
            _ => {}
        }
    }

    /// 终止单条执行中的命令：select 唤醒 → 工具返回「已终止」→
    /// 对话循环继续（模型看到结果后自行调整，不打断整轮）。
    /// 同步工具循环下，该会话当前待审批卡片必属此工具，一并撤下。
    fn cancel_tool(&self, chat_id: &str, tool_call_id: &str) {
        let running = {
            let mut tools = self.shared.running_tools.lock().unwrap();
            tools.remove(tool_call_id).filter(|r| r.chat_id == chat_id)
        };
        let Some(rt) = running else { return };
        let _ = rt.tx.send(());
        self.reject_pending_approvals(chat_id);
    }

    /// 撤下某会话全部待审批卡片（单条取消 / 整轮终止共用）
    fn reject_pending_approvals(&self, chat_id: &str) {
        let mut approvals = self.shared.approvals.lock().unwrap();
        let keys: Vec<String> = approvals
            .iter()
            .filter(|(_, p)| p.chat_id == chat_id)
            .map(|(k, _)| k.clone())
            .collect();
        let txs: Vec<oneshot::Sender<bool>> =
            keys.iter().filter_map(|k| approvals.remove(k)).map(|p| p.tx).collect();
        drop(approvals);
        for tx in txs {
            let _ = tx.send(false);
        }
    }

    /// 终止一轮进行中的对话：abort 任务（SSH lease 守卫随 future Drop 释放）、
    /// 撤下待审批卡片、补齐悬挂的 tool 结果（否则下轮请求历史不配对会被 API 拒绝）
    fn cancel_turn(&self, chat_id: &str) {
        let handle = {
            let mut turns = self.shared.turns.lock().unwrap();
            turns.remove(chat_id)
        };
        let running = match &handle {
            Some(h) => !h.is_finished(),
            None => false,
        };
        if let Some(h) = handle {
            h.abort();
        }
        if !running {
            return;
        }
        // 兜底清理 busy（abort 异步生效前的窗口期；guard Drop 时幂等）
        self.shared.busy.lock().unwrap().remove(chat_id);
        // 清理执行中工具的取消通道（abort 丢弃 future 时其自清理不会执行；
        // 只移除本会话条目，其他会话不受影响）
        {
            let mut tools = self.shared.running_tools.lock().unwrap();
            let keys: Vec<String> = tools
                .iter()
                .filter(|(_, r)| r.chat_id == chat_id)
                .map(|(k, _)| k.clone())
                .collect();
            let rts: Vec<RunningTool> =
                keys.iter().filter_map(|k| tools.remove(k)).collect();
            drop(tools);
            for rt in rts {
                let _ = rt.tx.send(());
            }
        }
        // 撤下该会话的待审批卡片 → 工具收到「拒绝」，对话循环自然收尾
        // （只移除本会话的条目，不影响其他会话的待审批项）
        self.reject_pending_approvals(chat_id);
        // 给悬挂的 tool_calls 补结果，保持 LLM 历史配对
        {
            let mut chats = self.shared.chats.lock().unwrap();
            if let Some(history) = chats.get_mut(chat_id) {
                let answered: HashSet<&String> = history
                    .iter()
                    .filter(|m| m.role == "tool")
                    .filter_map(|m| m.tool_call_id.as_ref())
                    .collect();
                let dangling: Vec<String> = history
                    .iter()
                    .filter(|m| m.role == "assistant")
                    .flat_map(|m| m.tool_calls.iter().flatten())
                    .map(|tc| tc.id.clone())
                    .filter(|id| !answered.contains(id))
                    .collect();
                for id in dangling {
                    history.push(LlmMessage {
                        role: "tool".into(),
                        content: Some("（用户终止了本轮执行）".into()),
                        tool_calls: None,
                        tool_call_id: Some(id),
                    });
                }
            }
        }
        self.shared.out.send_frame(&ServerFrame::AiResult {
            m: AiResultM {
                chat_id: chat_id.to_string(),
                ok: false,
                text: String::new(),
                message: Some("已终止".into()),
            },
        });
    }

    pub fn handle_approve(&self, m: &AiApproveM) {
        if let Some(pending) = self.shared.approvals.lock().unwrap().remove(&m.req_id) {
            let _ = pending.tx.send(m.approved);
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

    /// 工具调用 → toolcore 统一执行（策略 + 审批 + 审计一致）。
    /// 事件两段推送：running（卡片带 spinner + 单条终止按钮）→ 终态（done/failed/cancelled + 摘要 + 输出）。
    /// 单条终止：select 竞争取消通道，命中时工具返回 tool-cancelled，对话循环继续。
    async fn run_tool(
        &self,
        chat_id: &str,
        tool_call_id: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> serde_json::Value {
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
        let detail = tool_detail(name, args);
        self.send_tool_event(chat_id, tool_call_id, name, &detail, "running", None, None);
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        self.shared.running_tools.lock().unwrap().insert(
            tool_call_id.to_string(),
            RunningTool { chat_id: chat_id.to_string(), tx: cancel_tx },
        );
        let result = tokio::select! {
            r = toolcore::execute(&ctx, &approver, name, args) => r,
            // 用户点了这张卡片的「终止」：软取消，模型看到结果后继续对话
            _ = cancel_rx => serde_json::json!({
                "ok": false, "code": "tool-cancelled", "message": "用户终止了该命令执行"
            }),
        };
        self.shared.running_tools.lock().unwrap().remove(tool_call_id);
        let ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let stage = match result.get("code").and_then(|v| v.as_str()) {
            Some("tool-cancelled") => "cancelled",
            _ if ok => "done",
            _ => "failed",
        };
        self.send_tool_event(
            chat_id,
            tool_call_id,
            name,
            &detail,
            stage,
            Some(tool_result_summary(&result)),
            tool_output(&result),
        );
        result
    }

    fn send_tool_event(
        &self,
        chat_id: &str,
        tool_call_id: &str,
        name: &str,
        detail: &Option<String>,
        stage: &'static str,
        summary: Option<String>,
        output: Option<String>,
    ) {
        self.shared.out.send_frame(&ServerFrame::AiTool {
            m: AiToolM {
                chat_id: chat_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
                name: name.to_string(),
                detail: detail.clone(),
                stage,
                summary,
                output,
            },
        });
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
                    let result = self.run_tool(chat_id, &tc.id, &tc.function.name, &args).await;
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

/// 工具事件的操作摘要（命令文本或路径；不含文件内容等敏感大段数据）
fn tool_detail(name: &str, args: &serde_json::Value) -> Option<String> {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|v| v.to_string());
    match name {
        "exec_command" | "local_exec" => s("command"),
        "make_dir" | "read_file" | "write_file" | "delete_path" | "local_read_file" | "local_write_file" => {
            s("path")
        }
        "rename_path" => Some(format!("{} → {}", s("from")?, s("to").unwrap_or_default())),
        "sftp_upload_file" | "sftp_download_file" => {
            Some(format!("{} ⇄ {}", s("localPath")?, s("remotePath")?))
        }
        "list_dir" => s("path"),
        _ => None,
    }
}

/// 工具事件的结果摘要（退出码 / stderr 要点 / 字节数 / 错误原因）
fn tool_result_summary(result: &serde_json::Value) -> String {
    if let Some(hosts) = result.get("hosts").and_then(|v| v.as_array()) {
        return format!("{} 台主机", hosts.len());
    }
    if let Some(entries) = result.get("entries").and_then(|v| v.as_array()) {
        return format!("{} 项", entries.len());
    }
    if result.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        let msg = result.get("message").and_then(|v| v.as_str()).unwrap_or("未知错误");
        return format!("失败: {}", truncate(msg, 160));
    }
    if let Some(code) = result.get("exitCode").and_then(|v| v.as_i64()) {
        let mut s = format!("退出码 {code}");
        if result.get("timedOut").and_then(|v| v.as_bool()) == Some(true) {
            s.push_str("（超时）");
        }
        if let Some(stderr) = result.get("stderr").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
            s.push_str(" · ");
            s.push_str(&truncate(stderr.trim(), 120));
        }
        return s;
    }
    if let Some(bytes) = result.get("bytes").and_then(|v| v.as_u64()) {
        return format!("{bytes} 字节");
    }
    "成功".into()
}

/// 工具事件的可展开输出（exec 的 stdout/stderr；错误取 message。
/// 只取这几类字段——read_file 等的文件内容不外放到卡片）
fn tool_output(result: &serde_json::Value) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(stdout) = result.get("stdout").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        parts.push(stdout.to_string());
    }
    if let Some(stderr) = result.get("stderr").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        parts.push(format!("[stderr]\n{stderr}"));
    }
    if parts.is_empty() {
        return result
            .get("message")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| truncate(s, 2048));
    }
    Some(truncate(&parts.join("\n"), 4096))
}
