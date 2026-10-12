//! 会话帧协议（与 packages/protocol TS 定义一一对应）：
//! 控制帧（WS 文本帧 JSON，tag="t"）+ 数据帧（WS 二进制帧 [type:1][sidLen:1][sid][payload]）。
use serde::{Deserialize, Serialize};

// ── 数据帧类型 ────────────────────────────────────────────────────────────
pub const DF_STDOUT: u8 = 0x01;
pub const DF_STDIN: u8 = 0x02;
pub const DF_STDERR: u8 = 0x03;
pub const DF_SFTP_UPLOAD: u8 = 0x10;
pub const DF_SFTP_DOWNLOAD: u8 = 0x11;

pub fn encode_data_frame(frame_type: u8, sid: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + sid.len() + payload.len());
    out.push(frame_type);
    out.push(sid.len() as u8);
    out.extend_from_slice(sid.as_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn decode_data_frame(data: &[u8]) -> Option<(u8, String, &[u8])> {
    if data.len() < 2 {
        return None;
    }
    let frame_type = data[0];
    let sid_len = data[1] as usize;
    if data.len() < 2 + sid_len {
        return None;
    }
    let sid = String::from_utf8_lossy(&data[2..2 + sid_len]).into_owned();
    Some((frame_type, sid, &data[2 + sid_len..]))
}

/// SFTP 传输 payload：[opIdLen:1][opId:UTF-8][chunk]
pub fn encode_op_payload(op_id: &str, chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + op_id.len() + chunk.len());
    out.push(op_id.len() as u8);
    out.extend_from_slice(op_id.as_bytes());
    out.extend_from_slice(chunk);
    out
}

pub fn decode_op_payload(data: &[u8]) -> Option<(String, &[u8])> {
    if data.is_empty() {
        return None;
    }
    let len = data[0] as usize;
    if data.len() < 1 + len {
        return None;
    }
    let op_id = String::from_utf8_lossy(&data[1..1 + len]).into_owned();
    Some((op_id, &data[1 + len..]))
}

// ── 公共类型 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostAuth {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInput {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: HostAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostRecord {
    #[serde(flatten)]
    pub input: HostInput,
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
}

impl HostRecord {
    pub fn auth_password(&self) -> Option<&str> {
        match self.input.auth.kind.as_str() {
            "password" => self.input.auth.password.as_deref(),
            _ => None,
        }
    }

    pub fn auth_private_key(&self) -> Option<(&str, Option<&str>)> {
        match self.input.auth.kind.as_str() {
            "privateKey" => self
                .input
                .auth
                .private_key_path
                .as_deref()
                .map(|p| (p, self.input.auth.passphrase.as_deref())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub size_b: u64,
    pub mtime_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsDisk {
    pub source: String,
    pub target: String,
    pub size_b: f64,
    pub used_b: f64,
    pub used_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsProcess {
    pub pid: String,
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsSample {
    pub hostname: String,
    pub kernel: String,
    pub uptime_sec: i64,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub mem_total_b: f64,
    pub mem_used_b: f64,
    pub disks: Vec<MetricsDisk>,
    pub processes: Vec<MetricsProcess>,
    pub ts: String,
}

// ── Agent 控制 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct AgentOp {
    pub op: String,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentResult {
    pub op: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autostart: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_on_start: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ── 控制帧：客户端 → Agent ────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "t")]
pub enum ClientFrame {
    #[serde(rename = "open")]
    Open {
        s: String,
        m: OpenOptions,
    },
    #[serde(rename = "resize")]
    Resize {
        s: String,
        m: ResizeM,
    },
    #[serde(rename = "close")]
    Close {
        s: String,
        m: CloseM,
    },
    #[serde(rename = "confirm-hostkey")]
    ConfirmHostKey {
        s: String,
        m: ConfirmHostKeyM,
    },
    #[serde(rename = "hosts")]
    Hosts {
        m: serde_json::Value,
    },
    #[serde(rename = "sftp")]
    Sftp {
        s: String,
        m: SftpOpM,
    },
    #[serde(rename = "metrics")]
    Metrics {
        s: String,
        m: MetricsM,
    },
    #[serde(rename = "probe")]
    Probe {
        m: ProbeM,
    },
    #[serde(rename = "agent")]
    Agent {
        m: AgentOp,
    },
    #[serde(rename = "ai")]
    Ai {
        m: serde_json::Value,
    },
    #[serde(rename = "ai-approve")]
    AiApprove {
        m: AiApproveM,
    },
    #[serde(rename = "settings")]
    Settings {
        m: serde_json::Value,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOptions {
    pub host_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub cols: Option<u32>,
    #[serde(default)]
    pub rows: Option<u32>,
    #[serde(default)]
    pub term: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResizeM {
    pub cols: u32,
    pub rows: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloseM {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfirmHostKeyM {
    pub accepted: bool,
}

/// SFTP 操作（字段按 op 取用，保持与 TS 端鸭子类型的宽松度一致）
#[derive(Debug, Clone, Deserialize)]
pub struct SftpOpM {
    pub op: String,
    pub rid: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub op_id: Option<String>,
    #[serde(default)]
    pub recursive: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsM {
    #[serde(default)]
    pub interval_sec: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeM {
    pub host_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AiApproveM {
    pub chat_id: String,
    pub req_id: String,
    pub approved: bool,
}

// ── 控制帧：Agent → 客户端 ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "t")]
pub enum ServerFrame {
    #[serde(rename = "session-opened")]
    SessionOpened {
        s: String,
        m: SessionOpenedM,
    },
    #[serde(rename = "session-closed")]
    SessionClosed {
        s: String,
        m: SessionClosedM,
    },
    #[serde(rename = "hostkey-unknown")]
    HostKeyUnknown {
        s: String,
        m: HostKeyInfo,
    },
    #[serde(rename = "hosts")]
    Hosts {
        m: HostsResultM,
    },
    #[serde(rename = "sftp-result")]
    SftpResult {
        s: String,
        m: SftpResultM,
    },
    #[serde(rename = "sftp-progress")]
    SftpProgress {
        s: String,
        m: SftpProgressM,
    },
    #[serde(rename = "metrics-data")]
    MetricsData {
        s: String,
        m: MetricsSample,
    },
    #[serde(rename = "probe-result")]
    ProbeResult {
        m: ProbeResultM,
    },
    #[serde(rename = "agent-result")]
    AgentResult {
        m: AgentResult,
    },
    #[serde(rename = "ai-status")]
    AiStatus {
        m: AiStatusM,
    },
    #[serde(rename = "ai-delta")]
    AiDelta {
        m: AiDeltaM,
    },
    #[serde(rename = "ai-result")]
    AiResult {
        m: AiResultM,
    },
    #[serde(rename = "ai-approval")]
    AiApproval {
        m: AiApprovalM,
    },
    #[serde(rename = "settings-result")]
    SettingsResult {
        m: SettingsResultM,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        s: Option<String>,
        m: ErrorM,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOpenedM {
    pub host_id: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionClosedM {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostKeyInfo {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostsResultM {
    pub op: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<HostRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<HostRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpResultM {
    pub op: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<SftpEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_b: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpProgressM {
    pub op_id: String,
    pub transferred: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResultM {
    pub host_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiStatusM {
    #[serde(rename = "chatId")]
    pub chat_id: String,
    pub stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiDeltaM {
    #[serde(rename = "chatId")]
    pub chat_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiResultM {
    #[serde(rename = "chatId")]
    pub chat_id: String,
    pub ok: bool,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiApprovalM {
    pub chat_id: String,
    pub req_id: String,
    pub host: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsResultM {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorM {
    pub code: String,
    pub message: String,
}

impl ServerFrame {
    pub fn error(session: Option<&str>, code: &str, message: impl Into<String>) -> ServerFrame {
        ServerFrame::Error {
            s: session.map(|s| s.to_string()),
            m: ErrorM {
                code: code.to_string(),
                message: message.into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_control_frames() {
        let settings: ClientFrame = serde_json::from_str(r#"{"t":"settings","m":{"op":"set","key":"k","value":{"a":1}}}"#).unwrap();
        assert!(matches!(settings, ClientFrame::Settings { .. }));
        let hosts: ClientFrame = serde_json::from_str(r#"{"t":"hosts","m":{"op":"list"}}"#).unwrap();
        assert!(matches!(hosts, ClientFrame::Hosts { .. }));
        let ai: ClientFrame = serde_json::from_str(r#"{"t":"ai","m":{"op":"chat","chatId":"c1","text":"hi"}}"#).unwrap();
        assert!(matches!(ai, ClientFrame::Ai { .. }));
    }

    #[test]
    fn host_record_serializes_camel_case() {
        let rec = HostRecord {
            input: HostInput {
                name: "n".into(),
                host: "h".into(),
                port: 22,
                username: "u".into(),
                auth: HostAuth { kind: "password".into(), password: Some("p".into()), private_key_path: None, passphrase: None },
                tags: None,
            },
            id: "id".into(),
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let json = serde_json::to_value(&rec).unwrap();
        assert_eq!(json["privateKeyPath"], serde_json::Value::Null);
        assert_eq!(json["createdAt"], "t");
    }
}
