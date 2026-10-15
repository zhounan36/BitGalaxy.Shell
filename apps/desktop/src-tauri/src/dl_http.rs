//! 拖出下载端点：文件管理面板把远端文件拖到资源管理器时，Chromium 的
//! DownloadURL 拖出协议会让目标文件夹向本端点发起 GET，我们把远端文件
//! 经 SFTP 流式写回（不整读入内存）。挂在 Agent 既有 WS 端口上：
//! handle_conn 先 peek 首包，非 /dl 请求不消费字节、原样交回 WS 握手。
//!
//! 安全面：仅监听 127.0.0.1；能力令牌（uuid v4）按会话签发、30 分钟 TTL，
//! 可在 TTL 内重复使用（下载管理器可能重试）。下载权限面与面板本身一致
//! （会话用户可读的文件都可取）。

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::pool::ConnRegistry;
use crate::sftp::{open_read_deadline, stat_deadline};

/// 流式回写块大小（与 sftp.rs 下载路径一致）
const CHUNK: usize = 64 * 1024;
/// 令牌有效期：一次文件管理会话内可反复拖出，过期后重新 dl-open 签发
const TOKEN_TTL: Duration = Duration::from_secs(30 * 60);
/// 请求头上限（拖出 URL 只带 p/n 两个参数，16KB 足够）
const HEAD_LIMIT: usize = 16 * 1024;
/// 请求头读取超时
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);
/// 单块回写超时（客户端中断即失败退出）
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

struct DlCap {
    host_id: String,
    expires: Instant,
}

/// 拖出下载能力令牌表（进程级单例：签发方为 sftp 会话，消费方为 HTTP 端点）
pub struct DlTickets {
    port: u16,
    tickets: StdMutex<HashMap<String, DlCap>>,
}

impl DlTickets {
    pub fn new(port: u16) -> DlTickets {
        DlTickets {
            port,
            tickets: StdMutex::new(HashMap::new()),
        }
    }

    /// 签发能力令牌，返回下载基址（前端拖出时拼 ?p=远端路径&n=文件名）
    pub fn issue(&self, host_id: &str) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.tickets.lock().unwrap().insert(
            token.clone(),
            DlCap {
                host_id: host_id.to_string(),
                expires: Instant::now() + TOKEN_TTL,
            },
        );
        format!("http://127.0.0.1:{}/dl/{}", self.port, token)
    }

    /// 校验令牌（不消费：TTL 内允许重复下载）；顺手清理过期项防累积
    fn check(&self, token: &str) -> Option<String> {
        let now = Instant::now();
        let mut map = self.tickets.lock().unwrap();
        map.retain(|_, c| c.expires > now);
        map.get(token).map(|c| c.host_id.clone())
    }
}

/// 首包分流：GET /dl 前缀 → 本模块消费整条连接并返回 true；
/// 其余（WS 握手等）不消费任何字节、返回 false 交回调用方
pub async fn try_handle(
    stream: &mut TcpStream,
    tickets: &DlTickets,
    registry: &ConnRegistry,
) -> bool {
    let mut probe = [0u8; 16];
    let n = match stream.peek(&mut probe).await {
        Ok(n) => n,
        // 端点本身读不了首包：连接已坏，直接吞掉
        Err(_) => return true,
    };
    if !probe[..n].starts_with(b"GET /dl") {
        return false;
    }
    if let Err(e) = serve(stream, tickets, registry).await {
        write_simple(stream, 502, "Bad Gateway", &e).await;
    }
    true
}

/// 处理一次 /dl 下载：解析 → 鉴权 → stat/open（15s 超时）→ 流式回写
async fn serve(stream: &mut TcpStream, tickets: &DlTickets, registry: &ConnRegistry) -> Result<(), String> {
    let head = read_head(stream).await?;
    let req = parse_target(&head).ok_or_else(|| "请求格式错误".to_string())?;
    let host_id = tickets
        .check(&req.token)
        .ok_or_else(|| "下载令牌无效或已过期，请在文件面板重新发起".to_string())?;
    // 文件名缺省取远端路径末段（前端会显式传清洗后的 n）
    let name = req.name.clone().unwrap_or_else(|| {
        req.path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("download").to_string()
    });

    let lease = registry
        .acquire_sftp(&host_id, "drag-download")
        .await
        .map_err(|e| format!("连接远端失败：{}", e.message))?;
    let sftp = lease.sftp.as_ref();

    let meta = stat_deadline(sftp, &req.path).await.map_err(|e| {
        // 无读权限/不存在由 stat 即时回报；超时消息已含 "timeout" 便于分类
        if e.contains("No such file") || e.contains("not found") {
            "文件不存在".to_string()
        } else {
            e
        }
    })?;
    if meta.is_dir() {
        return Err("路径是目录（拖出仅支持单文件；目录请用面板内下载）".to_string());
    }
    let mut file = open_read_deadline(sftp, &req.path).await?;

    // 先发响应头（Content-Length 供下载管理器校验完整性；大小未知则无该头、流到关闭）
    let size = meta.size;
    let mut head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename*=UTF-8''{}\r\nConnection: close\r\n",
        rfc5987_encode(&name)
    );
    if let Some(sz) = size {
        head.push_str(&format!("Content-Length: {sz}\r\n"));
    }
    head.push_str("\r\n");
    write_all_timeout(stream, head.as_bytes()).await?;

    // 流式回写：远端读一块写一块，大文件不占内存；客户端断开即终止
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf).await.map_err(|e| format!("读取远端失败：{e}"))?;
        if n == 0 {
            break;
        }
        write_all_timeout(stream, &buf[..n]).await?;
    }
    // 读句柄随函数结束 drop（读路径无需 flush+close 收尾约定）；lease 同步释放
    Ok(())
}

async fn read_head(stream: &mut TcpStream) -> Result<String, String> {
    let mut raw: Vec<u8> = Vec::with_capacity(512);
    let mut buf = [0u8; 512];
    let deadline = tokio::time::sleep(HEAD_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        let n = tokio::select! {
            r = stream.read(&mut buf) => r.map_err(|e| format!("读取请求失败：{e}"))?,
            _ = &mut deadline => return Err("读取请求头 timeout (15s)".into()),
        };
        if n == 0 {
            return Err("连接提前关闭".into());
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > HEAD_LIMIT {
            return Err("请求头过大".into());
        }
        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(String::from_utf8_lossy(&raw).into_owned());
        }
    }
}

#[derive(Debug, PartialEq)]
struct DlRequest {
    token: String,
    path: String,
    name: Option<String>,
}

/// 解析请求行的 /dl/{token}?p=&n=（percent-decode 后返回）
fn parse_target(head: &str) -> Option<DlRequest> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    let token = path.strip_prefix("/dl/")?.to_string();
    let (mut p, mut n) = (None, None);
    for kv in query.unwrap_or_default().split('&').filter(|s| !s.is_empty()) {
        match kv.split_once('=') {
            Some(("p", v)) => p = Some(percent_decode(v)),
            Some(("n", v)) => n = Some(percent_decode(v)),
            _ => {}
        }
    }
    Some(DlRequest {
        token,
        path: p?,
        name: n,
    })
}

/// 严格 %XX 解码（前端 encodeURIComponent 产物；不处理 + → space）
fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // 逐字节处理避免在 UTF-8 中间切片 panic
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RFC 5987 ext-value 编码（Content-Disposition 文件名，支持中文）
fn rfc5987_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' => out.push(*b as char),
            b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn write_all_timeout(stream: &mut TcpStream, data: &[u8]) -> Result<(), String> {
    match tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(data)).await {
        Ok(r) => r.map_err(|e| format!("写回失败：{e}")),
        Err(_) => Err("写回 timeout (60s)".into()),
    }
}

async fn write_simple(stream: &mut TcpStream, code: u16, reason: &str, msg: &str) {
    let body = format!("{reason}：{msg}");
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = write_all_timeout(stream, resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_ascii_and_utf8() {
        assert_eq!(percent_decode("plain.txt"), "plain.txt");
        assert_eq!(percent_decode("a%20b.txt"), "a b.txt");
        assert_eq!(percent_decode("%E4%B8%AD%E6%96%87.txt"), "中文.txt");
        // 非法序列不 panic：非 %XX 保持原样，截断 UTF-8 经 lossy 替换为 U+FFFD
        assert_eq!(percent_decode("%zz%"), "%zz%");
        assert_eq!(percent_decode("%e4%b8"), "\u{FFFD}");
    }

    #[test]
    fn parse_target_full_and_missing_path() {
        let head = "GET /dl/tok-123?p=%2Fhome%2Fu%2Fa.txt&n=a.txt HTTP/1.1\r\nHost: x\r\n\r\n";
        let r = parse_target(head).unwrap();
        assert_eq!(r.token, "tok-123");
        assert_eq!(r.path, "/home/u/a.txt");
        assert_eq!(r.name.as_deref(), Some("a.txt"));

        // p 是必填：缺 p 无法定位远端文件
        assert!(parse_target("GET /dl/tok-123 HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_target("POST /dl/tok HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_target("GET /ws HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn tickets_issue_check_and_expiry() {
        let t = DlTickets::new(7777);
        let url = t.issue("host-1");
        assert!(url.starts_with("http://127.0.0.1:7777/dl/"));
        let token = url.rsplit('/').next().unwrap().to_string();
        assert_eq!(t.check(&token).as_deref(), Some("host-1"));
        assert_eq!(t.check("nope"), None);

        // 过期后校验失败且被清理
        t.tickets.lock().unwrap().get_mut(&token).unwrap().expires = Instant::now();
        assert_eq!(t.check(&token), None);
        assert!(t.tickets.lock().unwrap().is_empty());
    }

    #[test]
    fn rfc5987_keeps_attr_chars() {
        assert_eq!(rfc5987_encode("a-b_c.txt"), "a-b_c.txt");
        assert_eq!(rfc5987_encode("中 文.txt"), "%E4%B8%AD%20%E6%96%87.txt");
    }
}
