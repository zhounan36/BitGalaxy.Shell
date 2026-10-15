//! SQL 驱动宿主共享层：stdio NDJSON-RPC 协议类型 + 读写循环。
//!
//! 协议：stdin/stdout 每行一条 JSON。
//! 请求 `{"id": u64, "method": "...", "params": {...}}`；
//! 响应 `{"id": u64, "result": {...}}` 或 `{"id": u64, "error": {code, kind, message, retryable}}`；
//! 通知（无 id）如 `{"method": "log", "params": {"message": "..."}}`。

use std::future::Future;
use std::pin::Pin;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// 协议版本（initialize 契约）
pub const PROTOCOL_VERSION: u64 = 1;
/// maxRows 上限（协议契约：<= 2000）
pub const MAX_ROWS_LIMIT: u32 = 2000;

// ---- 错误 kind 取值（Agent 侧据此分类）----
pub const KIND_SQL_ERROR: &str = "sql_error";
pub const KIND_NETWORK: &str = "network";
pub const KIND_PERMISSION_DENIED: &str = "permission_denied";
pub const KIND_UNSUPPORTED: &str = "unsupported";

/// 错误对象（响应 error 字段）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: String,
    pub kind: String,
    pub message: String,
    pub retryable: bool,
}

/// 内部错误类型，最终序列化为 ErrorObject
#[derive(Debug, Clone)]
pub struct DriverError {
    pub code: String,
    pub kind: String,
    pub message: String,
    pub retryable: bool,
}

impl DriverError {
    pub fn new(code: impl Into<String>, kind: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            kind: kind.into(),
            message: message.into(),
            retryable,
        }
    }

    /// SQL 语法/业务错
    pub fn sql_error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, KIND_SQL_ERROR, message, false)
    }

    /// 连不上/断连/超时。注意：超时场景 message 必须含 "timeout"，Agent 才会归为 timeout 类
    pub fn network(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self::new(code, KIND_NETWORK, message, retryable)
    }

    /// 认证失败/权限拒绝。message 需含 "Access denied"/"authentication failed"/"permission denied"
    pub fn permission_denied(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, KIND_PERMISSION_DENIED, message, false)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("unsupported", KIND_UNSUPPORTED, message, false)
    }
}

impl From<DriverError> for ErrorObject {
    fn from(e: DriverError) -> Self {
        ErrorObject {
            code: e.code,
            kind: e.kind,
            message: e.message,
            retryable: e.retryable,
        }
    }
}

/// 一行输入解析出的请求/通知
#[derive(Debug, Clone)]
pub struct JsonLine {
    pub id: Option<u64>,
    pub method: String,
    pub params: Value,
}

/// 解析一行输入；非法 JSON 返回错误（调用方可回错误响应）
pub fn parse_line(line: &str) -> Result<JsonLine, DriverError> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| DriverError::new("bad-request", KIND_UNSUPPORTED, format!("invalid JSON: {e}"), false))?;
    let obj = v
        .as_object()
        .ok_or_else(|| DriverError::new("bad-request", KIND_UNSUPPORTED, "request must be a JSON object", false))?;
    let id = obj.get("id").and_then(|i| i.as_u64());
    let method = obj
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| DriverError::new("bad-request", KIND_UNSUPPORTED, "missing method", false))?
        .to_string();
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    Ok(JsonLine { id, method, params })
}

/// 成功响应行
pub fn encode_response_ok(id: u64, result: &Value) -> String {
    serde_json::to_string(&json!({ "id": id, "result": result })).expect("response serializable")
}

/// 失败响应行
pub fn encode_response_err(id: u64, err: &ErrorObject) -> String {
    serde_json::to_string(&json!({ "id": id, "error": err })).expect("error serializable")
}

/// 通知行（如 {"method":"log"}）
pub fn encode_notification(method: &str, params: &Value) -> String {
    serde_json::to_string(&json!({ "method": method, "params": params })).expect("notification serializable")
}

/// maxRows 参数钳制到 <= 2000；缺省 200
pub fn clamp_max_rows(v: Option<u64>) -> u32 {
    v.unwrap_or(200).min(MAX_ROWS_LIMIT as u64).max(1) as u32
}

/// 二进制 → JSON：base64 字符串（协议约定二进制列输出 base64）
pub fn bytes_to_json(bytes: &[u8]) -> Value {
    Value::String(B64.encode(bytes))
}

/// 整数转 JSON number，溢出时转 string
pub fn i64_to_json(v: i64) -> Value {
    Value::Number(v.into())
}

/// 无符号整数转 JSON number，超出 JSON 安全整数范围时转 string
pub fn u64_to_json(v: u64) -> Value {
    match serde_json::Number::from_f64(v as f64) {
        // 2^53 内精确表示才用 number
        _ if v <= (1u64 << 53) => Value::Number(v.into()),
        _ => Value::String(v.to_string()),
    }
}

/// 浮点转 JSON（NaN/Inf → null，JSON 不支持非有限值）
pub fn f64_to_json(v: f64) -> Value {
    serde_json::Number::from_f64(v).map(Value::Number).unwrap_or(Value::Null)
}

// ---- 读写循环 ----

pub type BoxFut = Pin<Box<dyn Future<Output = Result<Value, DriverError>> + Send>>;

/// 请求处理器：三个 bin 各自实现（内部持有连接表）
pub trait RequestHandler: Send + Sync + 'static {
    fn handle(&self, method: &str, params: Value) -> BoxFut;
}

/// stdin 逐行读 → 并发分发 → stdout 每行一条响应并 flush。
/// stdin EOF（管道关闭）后等待在途请求完成，随后优雅退出。
pub async fn run_stdio<H: RequestHandler>(handler: H) -> std::io::Result<()> {
    let (tx, mut rx) = mpsc::channel::<String>(64);

    // 单一写者任务：保证每行响应原子写出并 flush
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if out.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if out.write_all(b"\n").await.is_err() {
                break;
            }
            if out.flush().await.is_err() {
                break;
            }
        }
    });

    let handler = std::sync::Arc::new(handler);
    let mut inflight = tokio::task::JoinSet::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let parsed = match parse_line(&line) {
            Ok(p) => p,
            Err(e) => {
                let _ = tx.send(encode_response_err(0, &e.into())).await;
                continue;
            }
        };
        // 无 id 视为通知：忽略（Agent→驱动的 log 通知无需应答）
        let Some(id) = parsed.id else { continue };
        let h = handler.clone();
        let tx2 = tx.clone();
        inflight.spawn(async move {
            let resp = match h.handle(&parsed.method, parsed.params).await {
                Ok(result) => encode_response_ok(id, &result),
                Err(e) => encode_response_err(id, &e.into()),
            };
            let _ = tx2.send(resp).await;
        });
    }

    // 管道关闭：等在途请求跑完，再收尾写者
    while inflight.join_next().await.is_some() {}
    drop(tx);
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_roundtrip() {
        let line = r#"{"id":7,"method":"query","params":{"connId":"c1","sql":"select 1"}}"#;
        let p = parse_line(line).unwrap();
        assert_eq!(p.id, Some(7));
        assert_eq!(p.method, "query");
        assert_eq!(p.params["connId"], "c1");
    }

    #[test]
    fn parse_notification_has_no_id() {
        let p = parse_line(r#"{"method":"log","params":{"message":"hi"}}"#).unwrap();
        assert_eq!(p.id, None);
        assert_eq!(p.method, "log");
    }

    #[test]
    fn parse_bad_json_is_error() {
        assert!(parse_line("not json").is_err());
    }

    #[test]
    fn response_shapes_match_contract() {
        let ok = encode_response_ok(3, &json!({"ok": true}));
        let v: Value = serde_json::from_str(&ok).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["result"]["ok"], true);

        let err = DriverError::permission_denied("auth-failed", "authentication failed for user");
        let line = encode_response_err(4, &err.clone().into());
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["error"]["code"], "auth-failed");
        assert_eq!(v["error"]["kind"], "permission_denied");
        assert_eq!(v["error"]["retryable"], false);
        assert!(v["error"]["message"].as_str().unwrap().contains("authentication failed"));
    }

    #[test]
    fn notification_shape() {
        let line = encode_notification("log", &json!({"message": "m"}));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["method"], "log");
        assert!(v.get("id").is_none());
    }

    #[test]
    fn clamp_max_rows_limits() {
        assert_eq!(clamp_max_rows(None), 200);
        assert_eq!(clamp_max_rows(Some(50)), 50);
        assert_eq!(clamp_max_rows(Some(99999)), 2000);
    }

    #[test]
    fn value_conversions() {
        assert_eq!(bytes_to_json(&[0u8, 1, 2]), Value::String("AAEC".to_string()));
        assert_eq!(i64_to_json(-5), Value::Number((-5).into()));
        // 超出 2^53 转 string
        assert_eq!(u64_to_json(1u64 << 60), Value::String((1u64 << 60).to_string()));
        assert_eq!(u64_to_json(42), Value::Number(42u64.into()));
        assert_eq!(f64_to_json(f64::NAN), Value::Null);
    }
}
