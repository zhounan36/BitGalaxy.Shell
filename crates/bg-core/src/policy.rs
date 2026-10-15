//! MCP 工具安全策略（ai-policy.json）与命令分类器。
//! 优先级：硬黑名单 > 用户黑名单 > 白名单 > defaultAction。
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::stores::Stores;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecPolicy {
    pub enabled: bool,
    #[serde(rename = "defaultAction")]
    pub default_action: PolicyAction,
    #[serde(rename = "allowPatterns", default)]
    pub allow_patterns: Vec<String>,
    #[serde(rename = "denyPatterns", default)]
    pub deny_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WritePolicy {
    pub enabled: bool,
    #[serde(rename = "defaultAction")]
    pub default_action: PolicyAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsPolicy {
    #[serde(rename = "execTimeoutMs")]
    pub exec_timeout_ms: u64,
    #[serde(rename = "maxExecOutputBytes")]
    pub max_exec_output_bytes: usize,
    #[serde(rename = "maxReadFileBytes")]
    pub max_read_file_bytes: usize,
    #[serde(rename = "idleCloseSec")]
    pub idle_close_sec: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiPolicy {
    pub version: u32,
    pub exec: ExecPolicy,
    pub write: WritePolicy,
    pub limits: LimitsPolicy,
}

/// 内置硬黑名单：永不可关，优先于一切白名单
pub const BUILTIN_DENY_PATTERNS: &[&str] = &[
    r"\brm\s+[^;|&]*-(?:[a-zA-Z]*r[a-zA-Z]*f|[a-zA-Z]*f[a-zA-Z]*r)[a-zA-Z]*\s+(/|~)(?:\s|/|$|\*)",
    r"\bmkfs(?:\.\w+)?\b",
    r"\bdd\s+[^;|&]*of=/dev/",
    r">\s*/dev/(?:sd|nvme|hd|vd|xvd)",
    r"\b(?:shutdown|reboot|halt|poweroff)\b",
    r"\binit\s+[06]\b",
    r":\(\)\s*\{[^}]*\}\s*;\s*:",
    r"\bchmod\s+[^;|&]*777\s+/(?:\s|$)",
    r"\bchown\s+[^;|&]*\s/(?:\s|$)[^;|&]*-R|\bchown\s+[^;|&]*-R[^;|&]*\s+/(?:\s|$)",
    r"\b(?:curl|wget)\b[^;|&]*\|\s*(?:sudo\s+)?(?:ba|z|da|k)?sh\b",
    r"\|\s*sudo\s+(?:ba)?sh\b",
];

pub fn default_policy() -> AiPolicy {
    AiPolicy {
        version: 1,
        exec: ExecPolicy {
            enabled: true,
            default_action: PolicyAction::Ask,
            allow_patterns: Vec::new(),
            deny_patterns: Vec::new(),
        },
        write: WritePolicy {
            enabled: true,
            default_action: PolicyAction::Ask,
        },
        limits: LimitsPolicy {
            exec_timeout_ms: 30_000,
            max_exec_output_bytes: 64 * 1024,
            max_read_file_bytes: 256 * 1024,
            idle_close_sec: 300,
        },
    }
}

/// 策略存取键（settings 表单键，value 为整个 AiPolicy 的 JSON）
pub const POLICY_KEY: &str = "aiPolicy";

/// 加载 AI 策略：SQLite(settings.aiPolicy) 优先；
/// 库中无记录但存在旧 ai-policy.json 时自动迁移入库（迁移后改名 .bak）；
/// 均无 → 默认策略。损坏 → 默认 + 审计回退事件。
pub fn load_ai_policy(data_dir: &Path, stores: &Stores) -> AiPolicy {
    let def = default_policy();
    // 1) SQLite 优先
    if let Some(raw) = stores.settings.get(POLICY_KEY) {
        return merge_policy(&def, &raw, stores);
    }
    // 2) 旧 ai-policy.json → 迁移入库（一次性）
    let file = data_dir.join("ai-policy.json");
    if let Ok(text) = std::fs::read_to_string(&file) {
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(raw) => {
                let merged = merge_policy(&def, &raw, stores);
                stores
                    .settings
                    .set(POLICY_KEY, &serde_json::to_value(&merged).unwrap_or_default());
                stores.audit.append(
                    "ai.policy-migrated",
                    serde_json::json!({ "from": "ai-policy.json", "to": "settings.aiPolicy" }),
                );
                // 迁移完成后改名 .bak，防止双源漂移
                let mut bak = file.clone().into_os_string();
                bak.push(".bak");
                let _ = std::fs::rename(&file, std::path::PathBuf::from(bak));
                return merged;
            }
            Err(e) => {
                stores
                    .audit
                    .append("ai.policy-fallback", serde_json::json!({ "reason": e.to_string() }));
                return def;
            }
        }
    }
    def
}

/// 保存策略到 SQLite（工作台设置页 / 后续 WS 通道调用）
#[allow(dead_code)] // 预留：工作台「AI 安全策略」设置页接入后移除
pub fn save_ai_policy(stores: &Stores, policy: &AiPolicy) {
    stores
        .settings
        .set(POLICY_KEY, &serde_json::to_value(policy).unwrap_or_default());
    stores.audit.append(
        "ai.policy-updated",
        serde_json::json!({
            "execEnabled": policy.exec.enabled,
            "execDefault": policy.exec.default_action,
            "writeEnabled": policy.write.enabled,
            "writeDefault": policy.write.default_action,
        }),
    );
}

/// 原始 JSON 按段字段级合并到默认策略；整体损坏时回退默认（带审计）
fn merge_policy(def: &AiPolicy, raw: &serde_json::Value, stores: &Stores) -> AiPolicy {
    let merged = AiPolicy {
        version: 1,
        exec: merge_section(&def.exec, raw.get("exec")),
        write: merge_section(&def.write, raw.get("write")),
        limits: merge_section(&def.limits, raw.get("limits")),
    };
    if raw.get("exec").is_none() && raw.get("write").is_none() && raw.get("limits").is_none() {
        stores.audit.append(
            "ai.policy-fallback",
            serde_json::json!({ "reason": "存储的策略 JSON 缺少全部配置段" }),
        );
        return def.clone();
    }
    merged
}

/// 字段级合并：默认值序列化后用原始 JSON 的同段字段覆盖（等价 Node 的 {...def, ...raw}）
fn merge_section<T: Serialize + serde::de::DeserializeOwned>(
    def: &T,
    raw: Option<&serde_json::Value>,
) -> T {
    let mut base = serde_json::to_value(def).unwrap_or(serde_json::Value::Null);
    if let (Some(fields), Some(base_obj)) = (raw.and_then(|v| v.as_object()), base.as_object_mut()) {
        for (k, v) in fields {
            base_obj.insert(k.clone(), v.clone());
        }
    }
    let fallback = serde_json::to_value(def).unwrap_or(serde_json::Value::Null);
    serde_json::from_value(base).unwrap_or_else(|_| serde_json::from_value(fallback).unwrap())
}

#[derive(Debug, Clone)]
pub struct CommandDecision {
    pub action: PolicyAction,
    pub reason: &'static str,
    pub pattern: Option<String>,
}

fn matches(command: &str, patterns: &[String]) -> Option<String> {
    for src in patterns {
        if let Ok(re) = Regex::new(&format!("(?i){src}")) {
            if re.is_match(command) {
                return Some(src.clone());
            }
        }
        // 用户配置的非法正则：跳过
    }
    None
}

pub fn classify_command(command: &str, policy: &AiPolicy) -> CommandDecision {
    let builtin: Vec<String> = BUILTIN_DENY_PATTERNS.iter().map(|s| s.to_string()).collect();
    let denied = matches(command, &builtin).or_else(|| matches(command, &policy.exec.deny_patterns));
    if let Some(p) = denied {
        return CommandDecision { action: PolicyAction::Deny, reason: "deny-pattern", pattern: Some(p) };
    }
    if let Some(p) = matches(command, &policy.exec.allow_patterns) {
        return CommandDecision { action: PolicyAction::Allow, reason: "allow-pattern", pattern: Some(p) };
    }
    // 内置只读白名单：仅当默认动作是 Ask 时生效（减少审批打扰；
    // 用户显式收紧 default=deny 或放宽 default=allow 时尊重用户设置）
    if policy.exec.default_action == PolicyAction::Ask && builtin_read_only_re().is_match(command) {
        return CommandDecision { action: PolicyAction::Allow, reason: "builtin-read-only", pattern: None };
    }
    CommandDecision { action: policy.exec.default_action, reason: "default", pattern: None }
}

/// 内置只读命令白名单（远程 Linux 场景；与原桌宠通道行为一致）
fn builtin_read_only_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*(?:sudo\s+)?(ls|ll|cat|head|tail|pwd|whoami|id|hostname|uname|uptime|date|echo|which|env|printenv|df|free|du|wc|stat|file|ps|top\s+-b|ip\s+(a|addr)|ifconfig|netstat|ss|lsof|grep|find|git\s+(status|log|diff)|systemctl\s+(status|list|is-active)|docker\s+(ps|images)|kubectl\s+get|cat\s+/etc/os-release|lsb_release)\b").unwrap()
    })
}
