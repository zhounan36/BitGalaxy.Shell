//! 统一工具错误：工具层（toolcore）/ 连接池（pool）/ SQL 会话（sql）共用的
//! 错误码与错误类型。错误码稳定（AI 可据此自纠），跨 crate 共享故放在 bg-core。
//!
//! 历史注：原本定义在 toolcore.rs，因 pool.rs（bg-ssh）的连接 API 也返回
//! `Result<_, ToolError>`，若留在工具层会造成 bg-ssh → bg-ai 的循环依赖，
//! 故下沉到 bg-core（2026-10 四 EXE 架构拆分 P1b）。

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
