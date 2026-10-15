//! SQL 操作错误分类（仿 classify_sftp_error）：
//! 驱动错误 kind 优先透传，其余按子串归类。
//! 前端 SqlErrorKind → 中文文案 + 建议动作。

/// 错误分类：sql_error / network / permission_denied / timeout / driver_crashed /
/// driver_unavailable / tunnel_failed / too_large / unknown
pub(crate) fn classify_sql_error(msg: &str) -> Option<String> {
    let m = msg.to_ascii_lowercase();
    let kind = if m.contains("driver_crashed") {
        "driver_crashed"
    } else if m.contains("driver_unavailable") {
        "driver_unavailable"
    } else if m.contains("tunnel_failed") {
        "tunnel_failed"
    } else if m.contains("too_large") {
        "too_large"
    } else if m.contains("timeout") || m.contains("timed out") || m.contains("超时") {
        "timeout"
    } else if m.contains("access denied")
        || m.contains("permission denied")
        || m.contains("not permitted")
        || m.contains("拒绝访问")
        || m.contains("密码错误")
        || m.contains("authentication failed")
    {
        "permission_denied"
    } else if m.contains("connection")
        || m.contains("broken pipe")
        || m.contains("reset")
        || m.contains("eof")
        || m.contains("network")
        || m.contains("channel closed")
    {
        "network"
    } else if m.contains("syntax") || m.contains("sql") || m.contains("错误的") {
        "sql_error"
    } else {
        return None;
    };
    Some(kind.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_errors() {
        assert_eq!(classify_sql_error("连接超时").as_deref(), Some("timeout"));
        assert_eq!(classify_sql_error("Access denied for user").as_deref(), Some("permission_denied"));
        assert_eq!(classify_sql_error("connection reset by peer").as_deref(), Some("network"));
        assert_eq!(classify_sql_error("You have an error in your SQL syntax").as_deref(), Some("sql_error"));
        assert_eq!(classify_sql_error("driver 已崩溃（driver_crashed）").as_deref(), Some("driver_crashed"));
        assert_eq!(classify_sql_error("随便什么").as_deref(), None);
    }
}
