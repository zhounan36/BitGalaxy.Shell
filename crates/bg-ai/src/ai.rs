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

use bg_core::policy::AiPolicy;
use bg_core::protocol::{AiApprovalM, AiApproveM, AiAskResultM, AiChatsM, AiConsoleM, AiDeltaM, AiHistoryM, AiInputM, AiResultM, AiStatusM, AiThinkingM, AiToolM, ServerFrame};
use bg_core::stores::Stores;
use bg_core::wire::OutHub;
use bg_ssh::pool::ConnRegistry;
use crate::toolcore::{self, ApproveFuture, Approver, ToolCtx};

const MAX_HISTORY: usize = 40;
const MAX_TOOL_ROUNDS: usize = 8;
const APPROVAL_TIMEOUT_MS: u64 = 120_000;
/// 敏感输入（sudo 密码）等待时长：给用户找密码留足时间
const INPUT_TIMEOUT_MS: u64 = 300_000;
/// 历史查询单次最多返回的消息条数（防止超大对话拖垮 WS 帧）
const HISTORY_LOAD_LIMIT: usize = 500;

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
        "风格：中文、简短、活泼但专业；汇报结果时给出要点总结，不要大段贴原文（超 20 行只说关键部分）。",
        "【核心权限】你没有直接执行任何命令的权限（不能远程执行，也不能在本机执行）。需要执行命令时，必须调用 propose_command 把候选命令提交给用户：命令会显示在终端窗口的候选列表里，用户点击后粘贴到对应终端、自己敲回车执行。每条命令单独调用一次 propose_command；多个步骤按顺序多次提交。需要提权就在命令里写 sudo（用户会在终端里输密码）。命令必须单行、非交互（交互式命令会卡住）；连续步骤可用 && 连接。",
        "propose_command 之后：简要告诉用户建议执行哪条、为什么；命令尚未执行，不要假装已执行、不要编造输出。用户执行后通常会把输出贴回来，基于贴回的输出继续分析。",
        "远程查询工具（只读，可直接调用）：list_hosts 查主机；probe_host 测连通；get_metrics 采指标；list_dir 列远程目录；read_file 读远程小文件。",
        "文件类工具（写操作走审批）：write_file 写远程文件；local_read_file/local_write_file 读写本机文件；sftp_upload_file/sftp_download_file 上传下载。没有专用工具的操作（建目录 mkdir -p、删除 rm -rf、移动 mv、重启服务等）一律用 propose_command 提议。",
        "propose_command 参数：target=ssh 表示在远程主机终端执行（host 填主机名，帮用户匹配终端）；target=local 表示用户本机 PowerShell（打包用 Compress-Archive 或 tar），本机命令只会复制到剪贴板。",
        "典型流程：用户说「打包 X 上传到服务器」→ sftp_upload_file 上传（若本机还没有包，先 propose_command target=local 打包，等用户贴回结果）→ propose_command target=ssh 解压/运行。",
        "安全：写文件、上传下载会弹卡片请求用户批准，被拒时如实告知用户。文件大小超限时建议分卷或改用压缩。",
        "主机 id 必须先用 list_hosts 查询；用户说\"web 服务器\"这类称呼时按名称/标签匹配。",
        "路径规则：SFTP 工具（list_dir/read_file/write_file 等）不支持 ~ 波浪号展开（会被当作字面目录名）；相对路径相对登录用户家目录。不确定实际位置时先用 list_dir 查看 resolvedPath。",
        "用户贴回的命令输出就是执行结果：如实总结要点（退出码/stderr）；不要编造输出，不要把失败说成被拒绝。",
    ]
    .join("\n")
}

// ── 桌宠审批实现（弹卡片；超时视为拒绝，fail-closed）──────────────────────

/// 待审批项：记录归属会话，便于「终止对话」时精确撤下对应卡片
struct PendingApproval {
    chat_id: String,
    tx: oneshot::Sender<bool>,
}

/// 待输入项（sudo 密码等）：None = 用户取消/超时
struct PendingInput {
    chat_id: String,
    tx: oneshot::Sender<Option<String>>,
}

struct PetApprover {
    chat_id: String,
    out: OutHub,
    approvals: Arc<StdMutex<HashMap<String, PendingApproval>>>,
    inputs: Arc<StdMutex<HashMap<String, PendingInput>>>,
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

    fn request_secret(&self, title: &str, prompt: &str) -> toolcore::SecretFuture {
        let chat_id = self.chat_id.clone();
        let out = self.out.clone();
        let inputs = self.inputs.clone();
        let title = title.to_string();
        let prompt = prompt.to_string();
        Box::pin(async move {
            let req_id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel::<Option<String>>();
            inputs.lock().unwrap().insert(req_id.clone(), PendingInput { chat_id: chat_id.clone(), tx });
            out.send_frame(&ServerFrame::AiInput {
                m: AiInputM {
                    chat_id,
                    req_id: req_id.clone(),
                    title,
                    prompt,
                    secret: true,
                },
            });
            let value = match tokio::time::timeout(Duration::from_millis(INPUT_TIMEOUT_MS), rx).await {
                Ok(Ok(v)) => v,
                // 超时=取消；清理登记避免 map 泄漏
                _ => None,
            };
            inputs.lock().unwrap().remove(&req_id);
            Ok(value)
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
    inputs: Arc<StdMutex<HashMap<String, PendingInput>>>,
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
    /// 单条终止唤醒
    tx: oneshot::Sender<()>,
    /// 控制台介入输入（console_command 工具独有；其他工具为 None）
    console: Option<tokio::sync::mpsc::UnboundedSender<String>>,
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
                inputs: Arc::new(StdMutex::new(HashMap::new())),
                chats: StdMutex::new(HashMap::new()),
                busy: StdMutex::new(HashSet::new()),
                turns: StdMutex::new(HashMap::new()),
                running_tools: StdMutex::new(HashMap::new()),
            }),
        }
    }

    /// 新 WS 连接注册：AI 帧开始向该连接广播
    pub fn register_conn(&self, out: bg_core::wire::Outbound) {
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
            "console-input" => {
                // 用户介入 SSH 控制台（console_command 运行中）：文本直接写入 PTY stdin
                let tool_call_id =
                    m.get("toolCallId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let text = m.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let tools = self.shared.running_tools.lock().unwrap();
                if let Some(rt) = tools.get(&tool_call_id).filter(|r| r.chat_id == chat_id) {
                    if let Some(tx) = &rt.console {
                        let _ = tx.send(text);
                    }
                }
            }
            "input-reply" => {
                // sudo 密码等敏感输入回包：value 缺省/空 = 取消
                let req_id = m.get("reqId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let value = m.get("value").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
                if let Some(pending) = self.shared.inputs.lock().unwrap().remove(&req_id) {
                    let _ = pending.tx.send(value);
                }
            }
            "history" => {
                // chatId 缺省 = 最近一次对话（重开应用后恢复上次的聊天记录）
                let chat_id = match m.get("chatId").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                    Some(c) => c.to_string(),
                    None => match self.shared.stores.chat.latest_chat() {
                        Some(c) => c,
                        None => return, // 还没有任何记录
                    },
                };
                let messages = self.shared.stores.chat.list(&chat_id, HISTORY_LOAD_LIMIT as i64);
                self.shared.out.send_frame(&ServerFrame::AiHistory {
                    m: AiHistoryM { chat_id, messages },
                });
            }
            "history-list" => {
                // 历史会话列表（管理面板：复原 / 删除）
                let chats = self.shared.stores.chat.list_chats(HISTORY_LOAD_LIMIT as i64);
                self.shared.out.send_frame(&ServerFrame::AiChats { m: AiChatsM { chats } });
            }
            "history-delete" => {
                // 删除整个会话：先终止进行中的轮次并清掉内存上下文，再删记录，
                // 最后回最新的列表让前端刷新（当前会话被删时前端自行换新 id）
                let target = m.get("chatId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if target.is_empty() {
                    return;
                }
                self.cancel_turn(&target);
                self.shared.chats.lock().unwrap().remove(&target);
                self.shared.stores.chat.delete_chat(&target);
                let chats = self.shared.stores.chat.list_chats(HISTORY_LOAD_LIMIT as i64);
                self.shared.out.send_frame(&ServerFrame::AiChats { m: AiChatsM { chats } });
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
            // 一次性问答（SQL 终端右键 AI 等场景）：无对话历史、无工具、
            // 不落库，reqId 关联回包（ai-ask-result）——与 chat 轮次互不干扰
            "ask" => {
                let req_id = m.get("reqId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let text = m.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let system = m
                    .get("system")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string());
                if req_id.is_empty() || text.trim().is_empty() {
                    return;
                }
                let this = self.clone();
                tokio::spawn(async move {
                    this.run_ask(req_id, text, system).await;
                });
            }
            _ => {}
        }
    }

    /// 一次性问答：单次 LLM 补全（system 可由调用方指定），结果按 reqId 广播
    async fn run_ask(&self, req_id: String, text: String, system: Option<String>) {
        let fail = |message: String| {
            ServerFrame::AiAskResult {
                m: AiAskResultM {
                    req_id: req_id.clone(),
                    ok: false,
                    text: String::new(),
                    message: Some(message),
                },
            }
        };
        let Some(cfg) = load_ai_config(&self.shared.stores) else {
            self.shared.out.send_frame(&fail(
                "还没有配置 AI 服务：在主程序「设置 → AI 助手」填写 API Key 后重试（也可设置环境变量 DEEPSEEK_API_KEY）".into(),
            ));
            return;
        };
        let mut messages = Vec::new();
        if let Some(sys) = system {
            messages.push(LlmMessage {
                role: "system".into(),
                content: Some(sys),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        messages.push(LlmMessage {
            role: "user".into(),
            content: Some(text),
            tool_calls: None,
            tool_call_id: None,
        });
        let client = match reqwest::Client::builder().timeout(Duration::from_secs(120)).build() {
            Ok(c) => c,
            Err(e) => {
                self.shared.out.send_frame(&fail(e.to_string()));
                return;
            }
        };
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let body = serde_json::json!({ "model": cfg.model, "messages": messages });
        let resp = match client.post(&url).bearer_auth(&cfg.api_key).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                self.shared.out.send_frame(&fail(format!("AI 服务请求失败: {e}")));
                return;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let detail = resp.text().await.unwrap_or_default();
            self.shared
                .out
                .send_frame(&fail(format!("AI 服务返回 {status}: {}", truncate(&detail, 300))));
            return;
        }
        let data: serde_json::Value = match resp.json().await {
            Ok(d) => d,
            Err(e) => {
                self.shared.out.send_frame(&fail(format!("AI 服务返回解析失败: {e}")));
                return;
            }
        };
        // 只取最终文本（一次性问答不消费 reasoning）
        let content = data
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        self.shared.out.send_frame(&ServerFrame::AiAskResult {
            m: AiAskResultM { req_id, ok: true, text: content, message: None },
        });
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
        self.reject_pending_inputs(chat_id);
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

    /// 撤下某会话全部待输入卡片（sudo 密码等），一律按「取消」处理
    fn reject_pending_inputs(&self, chat_id: &str) {
        let mut inputs = self.shared.inputs.lock().unwrap();
        let keys: Vec<String> = inputs
            .iter()
            .filter(|(_, p)| p.chat_id == chat_id)
            .map(|(k, _)| k.clone())
            .collect();
        let txs: Vec<oneshot::Sender<Option<String>>> =
            keys.iter().filter_map(|k| inputs.remove(k)).map(|p| p.tx).collect();
        drop(inputs);
        for tx in txs {
            let _ = tx.send(None);
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
        // 撤下该会话的待审批/待输入卡片 → 工具收到「拒绝/取消」，对话循环自然收尾
        // （只移除本会话的条目，不影响其他会话的待审批项）
        self.reject_pending_approvals(chat_id);
        self.reject_pending_inputs(chat_id);
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
        // 「已终止」是瞬态 UI 事件，不落库（多次停止会在历史恢复时堆叠成噪音）
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
            inputs: self.shared.inputs.clone(),
        };
        // 控制台桥：emit 把 PTY 输出增量发成 ai-console 流式帧；
        // input 由 handle_ai 的 "console-input" op 喂入（用户在控制台卡片介入输入）
        let (console_tx, mut console_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let emit: Box<dyn Fn(String) + Send + Sync> = {
            let this = self.clone();
            let chat_id = chat_id.to_string();
            let tcid = tool_call_id.to_string();
            Box::new(move |delta: String| this.send_console_delta(&chat_id, &tcid, delta))
        };
        let mut ctx = ToolCtx {
            stores: &self.shared.stores,
            policy: &self.shared.policy,
            registry: self.shared.registry.as_ref(),
            client: "pet",
            console: Some(toolcore::ConsoleCtx { emit, input: &mut console_rx }),
        };
        let detail = tool_detail(name, args);
        self.send_tool_event(chat_id, tool_call_id, name, &detail, "running", None, None);
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        self.shared.running_tools.lock().unwrap().insert(
            tool_call_id.to_string(),
            RunningTool { chat_id: chat_id.to_string(), tx: cancel_tx, console: Some(console_tx) },
        );
        let result = if name == "propose_command" {
            // AI 无权直接执行：候选命令广播到工作台终端，用户自己粘贴执行。
            // 不进 toolcore（没有可执行动作），但仍走 running→终态卡片与落库。
            self.emit_proposed_command(chat_id, args)
        } else {
            tokio::select! {
                r = toolcore::execute(&mut ctx, &approver, name, args) => r,
                // 用户点了这张卡片的「终止」：软取消，模型看到结果后继续对话
                _ = cancel_rx => serde_json::json!({
                    "ok": false, "code": "tool-cancelled", "message": "用户终止了该命令执行"
                }),
                // 兜底超时：SFTP/exec 底层挂起时不能让对话永久卡死
                // （传输类工具与 termUpload 一致给 10 分钟，其余 60s）
                _ = tokio::time::sleep(tool_budget(name)) => serde_json::json!({
                    "ok": false, "code": "tool-timeout",
                    "message": format!("工具 {name} 执行超时（{}），已中止；请检查主机网络后重试", tool_budget(name).as_secs())
                }),
            }
        };
        self.shared.running_tools.lock().unwrap().remove(tool_call_id);
        let ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let stage = match result.get("code").and_then(|v| v.as_str()) {
            Some("tool-cancelled") => "cancelled",
            _ if ok => "done",
            _ => "failed",
        };
        let summary = tool_result_summary(&result);
        let output = tool_output(&result);
        self.send_tool_event(
            chat_id,
            tool_call_id,
            name,
            &detail,
            stage,
            Some(summary.clone()),
            output.clone(),
        );
        // 工具终态落库（running 阶段不落，重开应用后还原为终态卡片）
        let tool_json = serde_json::json!({
            "id": tool_call_id,
            "name": name,
            "detail": detail,
            "stage": stage,
            "summary": summary,
            "output": output,
        });
        self.shared.stores.chat.append(chat_id, "tool", None, None, false, Some(&tool_json));
        result
    }

    /// propose_command 的处理：解析候选命令并发 ai-commands 帧（广播），
    /// 返回给 LLM 的结果里明确「尚未执行」。失败也返回结构化结果，走 failed 卡片。
    fn emit_proposed_command(&self, chat_id: &str, args: &serde_json::Value) -> serde_json::Value {
        let s = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|v| v.trim().to_string());
        let command = s("command").unwrap_or_default();
        if command.is_empty() {
            return serde_json::json!({
                "ok": false, "code": "bad-args", "message": "command 不能为空；请提供单行命令文本"
            });
        }
        let target = match s("target").as_deref() {
            Some("local") => "local".to_string(),
            // 缺省按 ssh 处理（远程是主要场景）
            _ => "ssh".to_string(),
        };
        let proposed = bg_core::protocol::AiCommandM {
            id: uuid::Uuid::new_v4().to_string(),
            command: command.clone(),
            purpose: s("purpose").filter(|v| !v.is_empty()),
            target: target.clone(),
            host: s("host").filter(|v| !v.is_empty()),
        };
        self.shared.out.send_frame(&ServerFrame::AiCommands {
            m: bg_core::protocol::AiCommandsM {
                chat_id: chat_id.to_string(),
                commands: vec![proposed],
            },
        });
        let where_at = if target == "local" { "本机（复制到剪贴板）" } else { "SSH 终端" };
        serde_json::json!({
            "ok": true,
            "proposed": true,
            "command": command,
            "target": target,
            "message": format!("候选命令已推送到终端窗口的候选列表（{where_at}）；命令尚未执行，等用户点击粘贴并自己敲回车。可提醒用户查看候选列表，用户执行后可能把输出贴回来。")
        })
    }

    /// 控制台输出增量（console_command 运行中）：前端控制台卡片实时追加显示
    fn send_console_delta(&self, chat_id: &str, tool_call_id: &str, delta: String) {
        self.shared.out.send_frame(&ServerFrame::AiConsole {
            m: AiConsoleM {
                chat_id: chat_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
                delta,
            },
        });
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
            let entry = chats.entry(chat_id.to_string()).or_insert_with(|| {
                // Agent 重启后内存历史为空：从 SQLite 恢复最近的问答上下文
                //（工具明细无法还原 tool_calls 配对，只取用户/助手文本，足够模型接上话头）
                self.seed_history_from_db(chat_id)
            });
            entry.push(LlmMessage {
                role: "user".into(),
                content: Some(text.to_string()),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        self.shared.stores.chat.append(chat_id, "user", Some(text), None, false, None);
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
        // 本轮全部思考内容（多轮工具循环合并，落库与前端展示一致以空行分隔）
        let mut thinking_acc = String::new();

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
                if !thinking_acc.is_empty() {
                    thinking_acc.push_str("\n\n");
                }
                thinking_acc.push_str(think);
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
                    text: full.clone(),
                    message: None,
                },
            });
            // 助手最终回复落库（含全部思考过程，重开应用后可展开查看）
            let thinking = if thinking_acc.is_empty() { None } else { Some(thinking_acc.as_str()) };
            self.shared.stores.chat.append(chat_id, "assistant", Some(&full), thinking, false, None);
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
        let message = message.into();
        // 失败原因也落库：重开应用后错误提示与对话一起还原
        self.shared.stores.chat.append(chat_id, "assistant", Some(&message), None, true, None);
        self.shared.out.send_frame(&ServerFrame::AiResult {
            m: AiResultM {
                chat_id: chat_id.to_string(),
                ok: false,
                text: String::new(),
                message: Some(message),
            },
        });
    }

    /// Agent 重启后首次收到该会话消息：从 SQLite 恢复最近上下文
    /// （只取用户/助手文本问答，跳过工具卡片与错误提示）
    fn seed_history_from_db(&self, chat_id: &str) -> Vec<LlmMessage> {
        self.shared
            .stores
            .chat
            .list(chat_id, MAX_HISTORY as i64)
            .into_iter()
            .filter(|r| !r.error && r.tool.is_none())
            .filter_map(|r| {
                let role = match r.role.as_str() {
                    "user" => "user",
                    "assistant" => "assistant",
                    _ => return None,
                };
                r.content
                    .filter(|c| !c.is_empty())
                    .map(|content| LlmMessage {
                        role: role.into(),
                        content: Some(content),
                        tool_calls: None,
                        tool_call_id: None,
                    })
            })
            .collect()
    }
}

/// 工具执行预算：传输/文件类给长预算（与 termUpload 10 分钟一致），其余 60s。
/// exec_command 因 sudo 提权可能等待用户输入密码（最长 5 分钟），同样给长预算。
/// 这是挂起兜底（select 分支），工具内部正常的短超时不受影响。
fn tool_budget(name: &str) -> Duration {
    const TRANSFER_TOOLS: &[&str] = &[
        "sftp_upload_file",
        "sftp_download_file",
        "write_file",
        "read_file",
        "local_read_file",
        "local_write_file",
        "exec_command",
        // 控制台命令可能长驻交互（top/tail -f）或等用户现场介入，同样给长预算
        "console_command",
    ];
    if TRANSFER_TOOLS.contains(&name) {
        Duration::from_secs(600)
    } else {
        Duration::from_secs(60)
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
        "exec_command" | "local_exec" | "console_command" | "propose_command" => s("command"),
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
    if result.get("proposed").and_then(|v| v.as_bool()) == Some(true) {
        return "已推送到终端候选列表（待用户执行）".into();
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
