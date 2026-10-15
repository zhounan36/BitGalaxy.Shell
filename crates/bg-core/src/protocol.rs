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
    #[serde(rename = "retry-auth")]
    RetryAuth {
        s: String,
        m: RetryAuthM,
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
    /// 测试连接（添加/编辑主机表单）：用表单凭据做一次真实 SSH 认证，不落主机档案
    #[serde(rename = "test-host")]
    TestHost {
        m: HostInput,
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
    /// SQL 数据库管理（sql-result/sql-progress 回包）
    #[serde(rename = "sql")]
    Sql {
        s: String,
        m: SqlOpM,
    },
    /// SQL 连接档案 CRUD（sql-conns 回包；op: list/create/update/delete）
    #[serde(rename = "sql-conns")]
    SqlConns {
        m: serde_json::Value,
    },
    /// 枚举本机串口（serial-ports 回包）
    #[serde(rename = "serial-list")]
    SerialList,
    /// 串口控制（DTR/RTS/Break/改波特率/信号查询，serial-ctl-result 回包）
    #[serde(rename = "serial-ctl")]
    SerialCtl { s: String, m: SerialCtlM },
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
    /// kind="serial" 时的串口参数（其他 kind 忽略）
    #[serde(default)]
    pub serial: Option<SerialOptions>,
}

/// 串口连接参数（与 serialport crate 枚举对应；除 port/baudRate 外均可缺省）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialOptions {
    pub port: String,
    #[serde(default = "default_serial_baud")]
    pub baud_rate: u32,
    /// 5-8，缺省 8
    #[serde(default = "default_serial_data_bits")]
    pub data_bits: u8,
    /// none / odd / even，缺省 none
    #[serde(default)]
    pub parity: Option<String>,
    /// 1 / 2，缺省 1
    #[serde(default)]
    pub stop_bits: Option<u8>,
    /// none / software / hardware，缺省 none
    #[serde(default)]
    pub flow_control: Option<String>,
    /// 打开时 DTR 预设电平（缺省 true；Arduino 复位/ESP32 下载模式依赖此预设）
    #[serde(default = "default_true")]
    pub dtr: bool,
    /// 打开时 RTS 预设电平（缺省 true）
    #[serde(default = "default_true")]
    pub rts: bool,
}

fn default_true() -> bool {
    true
}

/// serial-ctl 帧：串口控制操作（DTR/RTS/Break/改波特率/信号查询）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialCtlM {
    /// dtr / rts / break / set-baud / signals
    pub op: String,
    /// dtr/rts 的目标电平
    #[serde(default)]
    pub level: Option<bool>,
    /// break 持续时间（ms，缺省 250）
    #[serde(default)]
    pub ms: Option<u32>,
    /// set-baud 的新波特率
    #[serde(default)]
    pub baud_rate: Option<u32>,
}

fn default_serial_baud() -> u32 {
    115_200
}

fn default_serial_data_bits() -> u8 {
    8
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

/// retry-auth 回包：password 缺省/None = 用户取消重试
#[derive(Debug, Clone, Deserialize)]
pub struct RetryAuthM {
    #[serde(default)]
    pub password: Option<String>,
}

/// SFTP 操作（字段按 op 取用，保持与 TS 端鸭子类型的宽松度一致）。
/// to：原生传输目标路径（get-* = 本地保存路径；put-* = 远端目标路径）
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
    /// preflight：'upload' | 'download'
    #[serde(default)]
    pub direction: Option<String>,
    /// put-file/get-file：覆盖语义 = 全新目标（下载删 .part / 上传跳过 APPEND）
    #[serde(default)]
    pub fresh: Option<bool>,
    /// preflight：调用方已知的传输总大小提示（浏览器 File 对象无本地路径时）
    #[serde(default)]
    pub size_b: Option<u64>,
}

/// SQL 操作（字段按 op 取用，与 SftpOpM 同款宽松度）。
/// op：connect / disconnect / query / query-more / execute / cancel / introspect /
/// fetch-sqlite / drivers-list
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlOpM {
    pub op: String,
    #[serde(default)]
    pub rid: Option<String>,
    /// connect：数据库连接规格（密码只在本帧内传输一次，后端转入内存缓存）
    #[serde(default)]
    pub conn: Option<SqlConnSpecM>,
    /// query/execute/cancel/introspect/disconnect：connect 返回的连接 id
    #[serde(default)]
    pub conn_id: Option<String>,
    /// query/execute：SQL 文本
    #[serde(default)]
    pub sql: Option<String>,
    /// query-more / cancel：驱动侧查询句柄
    #[serde(default)]
    pub query_id: Option<String>,
    /// query/query-more：结果行数上限（默认 500，硬上限 2000）
    #[serde(default)]
    pub max_rows: Option<u32>,
    /// query/execute：后端超时预算 ms（默认 30s，上限 10min）
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// introspect：databases | tables | columns | indexes
    #[serde(default)]
    pub scope: Option<String>,
    /// introspect：库/模式名
    #[serde(default)]
    pub schema: Option<String>,
    /// introspect：表名
    #[serde(default)]
    pub table: Option<String>,
    /// fetch-sqlite：远端 .db/.sqlite 文件路径
    #[serde(default)]
    pub path: Option<String>,
}

/// 数据库连接规格（connect op）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlConnSpecM {
    /// mysql | mariadb | postgres | sqlite
    #[serde(rename = "type")]
    pub kind: String,
    /// 历史字段：SSH 隧道时代的主机档案 id（P2 直连改造后不再使用，保留兼容旧帧）
    #[serde(default)]
    pub host_id: String,
    /// 远端数据库地址（经隧道转发的目标；缺省 127.0.0.1）
    #[serde(default)]
    pub host: Option<String>,
    /// 远端数据库端口（缺省：mysql 3306 / postgres 5432）
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: Option<String>,
    /// 明文密码（仅此帧传输；后端仅存内存，不落盘不进日志）
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub database: Option<String>,
    /// sqlite：远端 .db 文件路径（经 SFTP 下载到临时目录只读查询）
    #[serde(default)]
    pub sqlite_path: Option<String>,
    /// 显示名（前端连接管理器用）
    #[serde(default)]
    pub name: Option<String>,
}

/// SQL 连接档案（sql-connections.db 持久化行；Navicat 风格连接管理）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlConnRecord {
    pub id: String,
    pub name: String,
    /// mysql | mariadb | postgres | sqlite
    #[serde(rename = "type")]
    pub kind: String,
    /// 目标库地址（sqlite 忽略）
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    /// 明文密码（沿用 hosts 表先例：Secrets 内存缓存无法跨启动）
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub database: String,
    /// sqlite：本地 .db 文件路径
    #[serde(default)]
    pub sqlite_path: String,
    pub created_at: String,
    pub updated_at: String,
}

/// sql-conns 帧回包
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlConnsResultM {
    pub op: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conns: Option<Vec<SqlConnRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conn: Option<SqlConnRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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
    #[serde(rename = "auth-retry")]
    AuthRetry {
        s: String,
        m: AuthRetryM,
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
    /// 测试连接回包（test-host 帧）
    #[serde(rename = "test-host-result")]
    TestHostResult {
        m: TestHostResultM,
    },
    #[serde(rename = "agent-result")]
    AgentResult {
        m: AgentResult,
    },
    #[serde(rename = "ai-status")]
    AiStatus {
        m: AiStatusM,
    },
    #[serde(rename = "ai-thinking")]
    AiThinking {
        m: AiThinkingM,
    },
    #[serde(rename = "ai-delta")]
    AiDelta {
        m: AiDeltaM,
    },
    #[serde(rename = "ai-result")]
    AiResult {
        m: AiResultM,
    },
    /// 一次性问答回包（ai op:"ask"，reqId 关联；无历史/无工具）
    #[serde(rename = "ai-ask-result")]
    AiAskResult {
        m: AiAskResultM,
    },
    #[serde(rename = "ai-approval")]
    AiApproval {
        m: AiApprovalM,
    },
    #[serde(rename = "ai-input")]
    AiInput {
        m: AiInputM,
    },
    #[serde(rename = "ai-tool")]
    AiTool {
        m: AiToolM,
    },
    #[serde(rename = "ai-commands")]
    AiCommands {
        m: AiCommandsM,
    },
    #[serde(rename = "ai-history")]
    AiHistory {
        m: AiHistoryM,
    },
    #[serde(rename = "ai-chats")]
    AiChats {
        m: AiChatsM,
    },
    #[serde(rename = "ai-console")]
    AiConsole {
        m: AiConsoleM,
    },
    #[serde(rename = "settings-result")]
    SettingsResult {
        m: SettingsResultM,
    },
    #[serde(rename = "serial-ports")]
    SerialPorts {
        m: SerialPortsM,
    },
    /// serial-ctl 回包
    #[serde(rename = "serial-ctl-result")]
    SerialCtlResult {
        s: String,
        m: SerialCtlResultM,
    },
    /// SQL 操作回包（sql 帧的 rid 关联）
    #[serde(rename = "sql-result")]
    SqlResult {
        s: String,
        m: SqlResultM,
    },
    /// SQL 连接档案 CRUD 回包
    #[serde(rename = "sql-conns-result")]
    SqlConnsResult {
        m: SqlConnsResultM,
    },
    /// SQL 长任务进度（驱动下载 / SQLite 文件下载）
    #[serde(rename = "sql-progress")]
    SqlProgress {
        s: String,
        m: SqlProgressM,
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
    /// kind=serial：打开时的 DTR 预设电平（前端状态条回显）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dtr: Option<bool>,
    /// kind=serial：打开时的 RTS 预设电平
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rts: Option<bool>,
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

/// auth-retry 帧：密码认证被拒，请求工作台弹窗重输密码
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthRetryM {
    pub username: String,
    pub host: String,
    pub port: u16,
    pub message: String,
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
    /// pwd op 附带：远端主机名（OSC 7 主机校验用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 失败分类（classify_sftp_error）：permission_denied / no_space / quota /
    /// file_exists / text_file_busy / network / unknown
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    /// preflight op 的预检报告
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preflight: Option<PreflightReportM>,
    /// dl-open op 附带：拖出下载能力基址（http://127.0.0.1:port/dl/{token}）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dl_base: Option<String>,
}

/// 预检单项（警告或阻断原因，code 供前端/AI 程序化判断）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightItemM {
    /// target_exists / no_space / fs_4gb_limit / no_permission / large_file /
    /// free_unknown / scan_truncated
    pub code: String,
    pub message: String,
}

/// 传输预检报告（上传方向：path=本地源、to=远端目标；下载方向：path=远端源、to=本地目标）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightReportM {
    /// 传输总字节数（目录扫描被截断时为下限）
    pub size_b: u64,
    /// 文件数（目录任务；单文件 = 1）
    pub file_count: u64,
    /// 目录扫描超预算被截断（sizeB 为下限）
    pub scan_truncated: bool,
    /// 上传：远端目标已存在（重名）
    pub remote_exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_size_b: Option<u64>,
    /// 远端可用字节数（服务器不支持 statvfs 扩展时缺省）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_free_b: Option<u64>,
    /// 远端目标目录可写（探针文件实测）
    pub remote_writable: bool,
    /// 下载：本地目标已存在
    pub local_exists: bool,
    /// 本地可用字节数（浏览器宿主无本地盘概念时缺省）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_free_b: Option<u64>,
    /// 本地卷格式（NTFS/FAT32/exFAT…）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_fs: Option<String>,
    /// 本地单文件大小上限（FAT32 = 4GiB-1；其他/未知缺省 = 不设限）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_max_file_b: Option<u64>,
    /// 重名时的不冲突建议路径（name(1).ext 递增）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_path: Option<String>,
    /// 警告（不阻断，但应让用户知情）
    pub warnings: Vec<PreflightItemM>,
    /// 阻断原因（满足任一即不应发起传输）
    pub blockers: Vec<PreflightItemM>,
}

/// 原生传输进度（opId 关联任务；新增字段对旧客户端透明——Option + skip）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpProgressM {
    pub op_id: String,
    pub transferred: u64,
    pub total: u64,
    /// 当前正在传输的文件（目录任务为相对路径）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 已完成文件数（目录任务）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_done: Option<u64>,
    /// 文件总数（目录任务；0 = 尚在扫描）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_total: Option<u64>,
    /// 平滑速度（字节/秒）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_bps: Option<u64>,
    /// 本次续传已跳过的字节数（断点续传起点，0 = 全新传输）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resumed_b: Option<u64>,
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

/// 测试连接回包：ok = SSH 建连 + 认证通过（密码/私钥有效）
#[derive(Debug, Clone, Serialize)]
pub struct TestHostResultM {
    pub ok: bool,
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
pub struct AiThinkingM {
    #[serde(rename = "chatId")]
    pub chat_id: String,
    pub delta: String,
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

/// 一次性问答回包（SQL 终端右键 AI 等场景）：reqId 关联，不经对话历史
#[derive(Debug, Clone, Serialize)]
pub struct AiAskResultM {
    #[serde(rename = "reqId")]
    pub req_id: String,
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

/// 敏感信息输入请求（如 sudo 密码）：弹卡片让用户输入，回包走 Ai op:"input-reply"
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiInputM {
    pub chat_id: String,
    pub req_id: String,
    pub title: String,
    pub prompt: String,
    /// true = 密码框（前端 type=password，不回显）
    pub secret: bool,
}

/// AI 工具调用事件（广播到所有连接；聊天里渲染为可折叠执行卡片）。
/// running 先推（卡片带 spinner + 单条终止按钮），终态再推一次同 toolCallId 更新。
#[derive(Debug, Clone, Serialize)]
pub struct AiToolM {
    #[serde(rename = "chatId")]
    pub chat_id: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    pub name: String,
    /// 操作摘要（命令文本 / 路径）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// running | done | failed | cancelled
    pub stage: &'static str,
    /// 一行结果摘要（退出码 / 字节数 / 错误要点）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// 可展开的完整输出（stdout+stderr 或错误详情，已截断）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// AI 候选命令（propose_command 工具的产物）：不直接执行，推给工作台终端
/// 由用户点击粘贴到终端、自己敲回车执行
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiCommandM {
    pub id: String,
    /// 单行命令文本
    pub command: String,
    /// 一句话用途说明
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// ssh | local：粘贴到 SSH 终端，或复制到剪贴板（本机命令不自动进终端）
    pub target: String,
    /// 目标主机名（target=ssh 时用于与当前终端匹配）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// 候选命令推送帧（广播；工作台终端窗口里渲染候选列表）
#[derive(Debug, Clone, Serialize)]
pub struct AiCommandsM {
    #[serde(rename = "chatId")]
    pub chat_id: String,
    pub commands: Vec<AiCommandM>,
}

/// 对话历史查询回包（op:"history"；chatId 缺省 = 最近一次对话）
#[derive(Debug, Clone, Serialize)]
pub struct AiHistoryM {
    #[serde(rename = "chatId")]
    pub chat_id: String,
    pub messages: Vec<crate::stores::ChatMessageRow>,
}

/// 历史会话列表（op:"history-list"；删除后也回此帧刷新列表）
#[derive(Debug, Clone, Serialize)]
pub struct AiChatsM {
    pub chats: Vec<crate::stores::ChatSummaryRow>,
}

/// SSH 控制台输出增量（console_command 运行中流式回传，前端实时追加显示）
#[derive(Debug, Clone, Serialize)]
pub struct AiConsoleM {
    #[serde(rename = "chatId")]
    pub chat_id: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    pub delta: String,
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

/// 本机串口描述（serial-ports 帧）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialPortM {
    /// 端口名（Windows: COM3；Unix: /dev/ttyUSB0）
    pub name: String,
    /// usb / bluetooth / pci / unknown
    pub kind: String,
    /// USB 设备描述（厂商 + 产品名，如 "wch.cn CH340"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
}

/// serial-ports 回包
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialPortsM {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<SerialPortM>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// 串口输入信号线状态（signals 查询结果）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialSignalsM {
    pub cd: bool,
    pub cts: bool,
    pub dsr: bool,
    pub ri: bool,
}

/// serial-ctl 回包
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialCtlResultM {
    pub ok: bool,
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// op=signals 时的信号线状态
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signals: Option<SerialSignalsM>,
}

// ── SQL（数据库管理）────────────────────────────────────────────────────

/// 结果集列描述
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlColumnM {
    pub name: String,
    /// 驱动报告的列类型名（mysql: VARCHAR…；postgres: int4…；sqlite: TEXT…）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nullable: Option<bool>,
}

/// introspect:columns 项（结构树节点）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all="camelCase")]
pub struct SqlColumnInfoM {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    pub pk: bool,
    pub nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// introspect:indexes 项
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlIndexM {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

/// introspect:databases 项
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlDatabaseM {
    pub name: String,
}

/// introspect:tables 项
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlTableM {
    pub name: String,
    /// table | view
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_estimate: Option<i64>,
}

/// SQL 操作回包（字段按 op 取用）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlResultM {
    pub op: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 失败分类（classify_sql_error）：sql_error / network / permission_denied /
    /// timeout / driver_crashed / driver_unavailable / tunnel_failed / too_large / unknown
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    /// connect：会话内连接 id（后续 query/introspect 引用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conn_id: Option<String>,
    /// connect：服务器版本
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// query/query-more：驱动侧查询句柄（query-more 引用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_id: Option<String>,
    /// query/execute：结果集列
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<SqlColumnM>>,
    /// query/query-more：行数据（JSON 值二维数组；NULL = null）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<Vec<serde_json::Value>>>,
    /// query/query-more：返回行数（= rows.len()）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
    /// 结果集被 maxRows 截断（还有更多行）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// execute：受影响行数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected: Option<u64>,
    /// 执行耗时 ms
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// introspect:databases
    #[serde(skip_serializing_if = "Option::is_none")]
    pub databases: Option<Vec<SqlDatabaseM>>,
    /// introspect:tables
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables: Option<Vec<SqlTableM>>,
    /// introspect:columns
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<SqlColumnInfoM>>,
    /// introspect:indexes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<SqlIndexM>>,
    /// drivers-list：已知驱动（name/version/state：installed / not-installed）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drivers: Option<Vec<SqlDriverInfoM>>,
    /// fetch-sqlite：下载+打开完成，本地临时文件句柄 id（后续 query 用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sqlite_ref: Option<String>,
}

/// drivers-list 项
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlDriverInfoM {
    pub name: String,
    pub version: String,
    /// installed | not-installed | downloading
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// SQL 长任务进度（驱动下载 / SQLite 文件下载）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlProgressM {
    pub op: String,
    /// 关联请求 rid（下载/安装哪个任务）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rid: Option<String>,
    /// 阶段说明（如 "下载远端文件"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// 已完成字节
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transferred: Option<u64>,
    /// 总字节（未知缺省）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
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
        let cancel: ClientFrame = serde_json::from_str(r#"{"t":"ai","m":{"op":"cancel","chatId":"c1"}}"#).unwrap();
        assert!(matches!(cancel, ClientFrame::Ai { .. }));
    }

    #[test]
    fn serial_ctl_frame_parses_camel_case() {
        // dtr/rts 电平、break 时长、set-baud 波特率、signals 查询（level/ms/baudRate 全可缺省）
        let dtr: ClientFrame =
            serde_json::from_str(r#"{"t":"serial-ctl","s":"s1","m":{"op":"dtr","level":false}}"#).unwrap();
        let ClientFrame::SerialCtl { s, m } = dtr else { panic!("want serial-ctl") };
        assert_eq!(s, "s1");
        assert_eq!(m.op, "dtr");
        assert_eq!(m.level, Some(false));

        let baud: ClientFrame =
            serde_json::from_str(r#"{"t":"serial-ctl","s":"s1","m":{"op":"set-baud","baudRate":9600}}"#).unwrap();
        let ClientFrame::SerialCtl { m, .. } = baud else { panic!("want serial-ctl") };
        assert_eq!(m.baud_rate, Some(9600));

        let sig: ClientFrame =
            serde_json::from_str(r#"{"t":"serial-ctl","s":"s1","m":{"op":"signals"}}"#).unwrap();
        let ClientFrame::SerialCtl { m, .. } = sig else { panic!("want serial-ctl") };
        assert_eq!(m.op, "signals");
        assert_eq!(m.level, None);

        // SerialOptions.dtr/rts 缺省 true（旧协议不带这两个字段保持开高电平）
        let opts: SerialOptions = serde_json::from_str(r#"{"port":"COM3","baudRate":115200}"#).unwrap();
        assert!(opts.dtr && opts.rts);
        let opts: SerialOptions =
            serde_json::from_str(r#"{"port":"COM3","baudRate":115200,"dtr":false,"rts":true}"#).unwrap();
        assert!(!opts.dtr && opts.rts);
    }

    #[test]
    fn serial_ctl_result_serializes_camel_case() {
        let frame = ServerFrame::SerialCtlResult {
            s: "s1".into(),
            m: SerialCtlResultM {
                ok: true,
                op: "signals".into(),
                message: None,
                signals: Some(SerialSignalsM { cd: false, cts: true, dsr: true, ri: false }),
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""t":"serial-ctl-result""#), "{json}");
        assert!(json.contains(r#""cts":true"#), "{json}");
        assert!(!json.contains("signals_m"), "{json}");
    }

    #[test]
    fn ai_tool_event_serializes_camel_case() {
        let frame = ServerFrame::AiTool {
            m: AiToolM {
                chat_id: "c1".into(),
                tool_call_id: "tc1".into(),
                name: "exec_command".into(),
                detail: Some("mkdir /home/test".into()),
                stage: "running",
                summary: None,
                output: None,
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""t":"ai-tool""#), "{json}");
        assert!(json.contains(r#""chatId":"c1""#), "{json}");
        assert!(json.contains(r#""toolCallId":"tc1""#), "{json}");
        assert!(json.contains(r#""stage":"running""#), "{json}");
        assert!(!json.contains("chat_id"), "{json}");
        assert!(!json.contains("tool_call_id"), "{json}");
    }

    #[test]
    fn ai_history_serializes_camel_case() {
        let frame = ServerFrame::AiHistory {
            m: AiHistoryM {
                chat_id: "c1".into(),
                messages: vec![crate::stores::ChatMessageRow {
                    id: 7,
                    chat_id: "c1".into(),
                    role: "tool".into(),
                    content: None,
                    thinking: None,
                    error: false,
                    tool: Some(serde_json::json!({"id":"tc1","stage":"done"})),
                    created_at: "t".into(),
                }],
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""t":"ai-history""#), "{json}");
        assert!(json.contains(r#""chatId":"c1""#), "{json}");
        assert!(json.contains(r#""toolCallId""#) || json.contains(r#""tool":"#), "{json}");
        assert!(!json.contains("chat_id"), "{json}");
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
