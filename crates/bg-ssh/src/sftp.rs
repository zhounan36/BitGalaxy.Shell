//! SFTP 文件管理（kind=sftp 会话）：目录操作 + 流式上传/下载（64KB 块）。
//! 上传按 opId 建有序命令队列（保持块顺序、不阻塞 WS 读循环）。
//!
//! 另含原生传输引擎（§ 原生传输）：数据在 Agent 进程内直传本地磁盘，
//! 不经过 WebView——断点续传（.part / 远端 APPEND）、目录递归并发、
//! 256KB 大块、节流进度 + 平滑速度、可取消。
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use russh_sftp::client::fs::{File, Metadata};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use bg_core::protocol::{SftpEntry, SftpProgressM, SftpResultM, ServerFrame, DF_SFTP_DOWNLOAD};
use bg_core::wire::Outbound;

const CHUNK: usize = 64 * 1024;

fn sftp_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// 本地（Windows）文件操作失败的消息前缀：与远端 SFTP 错误区分归类
const LOCAL_IO_PREFIXES: [&str; 8] = [
    "读取本地文件失败",
    "读取本地目录失败",
    "打开本地文件失败",
    "写本地文件失败",
    "重置部分文件失败",
    "落盘失败",
    "定位本地文件失败",
    "读本地文件失败",
];

/// SFTP v3 无专用错误状态码（ENOSPC/EACCES 等以 Failure+errno 文本返回），
/// 按子串归类；本地 IO 错误（Windows 中文文案）一并覆盖。
/// 返回：local_io / permission_denied / no_space / quota / file_exists / text_file_busy / network / unknown
/// 错误分类（AI 工具层复用，故 pub）
pub fn classify_sftp_error(msg: &str) -> Option<String> {
    // 本地文件操作失败（下载写 .part / 上传读源）与远端账号权限无关：
    // 单独归类，避免 Windows"拒绝访问 (os error 5)"被误映射为远端 permission_denied 建议
    if LOCAL_IO_PREFIXES.iter().any(|p| msg.contains(p)) {
        return Some("local_io".to_string());
    }
    let m = msg.to_ascii_lowercase();
    let kind = if m.contains("permission denied")
        || m.contains("access denied")
        || m.contains("拒绝访问")
        || m.contains("not permitted")
    {
        "permission_denied"
    } else if m.contains("no space left") || m.contains("磁盘空间不足") || m.contains("没有足够的磁盘空间") {
        "no_space"
    } else if m.contains("quota") {
        "quota"
    } else if m.contains("file exists") || m.contains("already exists") {
        "file_exists"
    } else if m.contains("text file busy") || m.contains("正在被另一进程使用") {
        "text_file_busy"
    } else if m.contains("no such file") || m.contains("找不到指定") || m.contains("系统找不到指定的路径") {
        "not_found"
    } else if m.contains("connection")
        || m.contains("broken pipe")
        || m.contains("timeout")
        || m.contains("timed out")
        || m.contains("reset")
        || m.contains("channel closed")
        || m.contains("eof")
    {
        "network"
    } else {
        return None;
    };
    Some(kind.to_string())
}

/// 远端预检结论（SftpController::remote_preflight 产出，由 session.rs 组装成报告）
pub(crate) struct RemoteCheck {
    pub exists: bool,
    pub is_dir: bool,
    pub size_b: Option<u64>,
    /// 远端目标目录可用字节数（statvfs 不支持时 None）
    pub free_b: Option<u64>,
    /// 远端目标目录可写（探针实测；下载方向恒 true）
    pub writable: bool,
    /// 重名建议路径（name(1).ext 递增）
    pub suggested_path: Option<String>,
}

/// POSIX 远端路径的父目录（"/a/b/c" → "/a/b"；根 → "/"）
pub(crate) fn dirname_remote(p: &str) -> String {
    match p.trim_end_matches('/').rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent.to_string(),
        _ => "/".to_string(),
    }
}

/// POSIX 远端路径的末段名
pub(crate) fn basename_remote(p: &str) -> &str {
    p.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
}

/// 预检目录扫描预算：防超大目录拖垮预检
pub(crate) struct ScanBudget {
    pub max_files: usize,
    pub deadline: Instant,
}

impl ScanBudget {
    pub fn new() -> ScanBudget {
        ScanBudget {
            max_files: 20_000,
            deadline: Instant::now() + Duration::from_secs(3),
        }
    }
    fn exhausted(&self, count: usize) -> bool {
        count >= self.max_files || Instant::now() > self.deadline
    }
}

/// 本地目录预检扫描：累计 (总字节, 文件数)；超预算停止并返回 truncated=true。
/// 软链接不跟随（与传输引擎的 collect 行为一致按普通处理，仅统计大小）。
pub(crate) fn scan_local_size(root: &Path, budget: &ScanBudget) -> Result<(u64, u64, bool), String> {
    let mut total: u64 = 0;
    let mut files: u64 = 0;
    let truncated = scan_local_inner(root, budget, &mut total, &mut files)?;
    Ok((total, files, truncated))
}

fn scan_local_inner(dir: &Path, budget: &ScanBudget, total: &mut u64, files: &mut u64) -> Result<bool, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("读取目录失败 {dir:?}: {e}"))?;
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if scan_local_inner(&entry.path(), budget, total, files)? {
                return Ok(true);
            }
        } else {
            *total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            *files += 1;
            if budget.exhausted(*files as usize) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// 远端目录预检扫描：累计 (总字节, 文件数)；超预算停止并返回 truncated=true。
pub(crate) async fn scan_remote_size(
    sftp: &SftpSession,
    root: &str,
    budget: &ScanBudget,
) -> Result<(u64, u64, bool), String> {
    let mut total: u64 = 0;
    let mut files: u64 = 0;
    let truncated = scan_remote_inner(sftp, root, budget, &mut total, &mut files).await?;
    Ok((total, files, truncated))
}

fn scan_remote_inner<'a>(
    sftp: &'a SftpSession,
    dir: &'a str,
    budget: &'a ScanBudget,
    total: &'a mut u64,
    files: &'a mut u64,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>> {
    Box::pin(async move {
        let rd = match sftp.read_dir(dir).await {
            Ok(rd) => rd,
            Err(e) => return Err(format!("读取远端目录失败 {dir}: {e}")),
        };
        // russh-sftp ReadDir 为同步 Iterator（条目已在 read_dir 时批量取回）
        for entry in rd {
            let attrs = entry.metadata();
            if attrs.is_dir() {
                let sub = join_remote(dir, &entry.file_name());
                if scan_remote_inner(sftp, &sub, budget, total, files).await? {
                    return Ok(true);
                }
            } else {
                *total += attrs.size.unwrap_or(0);
                *files += 1;
                if budget.exhausted(*files as usize) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    })
}


/// 快路径远端操作统一超时：池中连接可能已半开（远端已断、本地未感知），
/// 请求发出后永远等不到回应。15s 无响应报错（消息含 "timeout" 会被
/// classify_sftp_error 归为 network，上层驱逐池条目后下次自动重建）。
/// 权限被拒由服务器即时返回（毫秒级），不受该超时影响——两类失败可快速区分。
pub const SFTP_META_DEADLINE: Duration = Duration::from_secs(15);

/// stat 元数据（15s 超时）：死连接快速失败，权限/不存在即时返回
pub async fn stat_deadline(sftp: &SftpSession, path: &str) -> Result<Metadata, String> {
    match tokio::time::timeout(SFTP_META_DEADLINE, sftp.metadata(path)).await {
        Ok(r) => r.map_err(sftp_err),
        Err(_) => Err(format!("sftp stat timeout (15s): {path}")),
    }
}

/// 打开远端文件用于读取（15s 超时）
pub async fn open_read_deadline(sftp: &SftpSession, path: &str) -> Result<File, String> {
    match tokio::time::timeout(SFTP_META_DEADLINE, sftp.open(path)).await {
        Ok(r) => r.map_err(sftp_err),
        Err(_) => Err(format!("sftp open timeout (15s): {path}")),
    }
}

/// 打开远端文件用于写入（15s 超时）：append=true 走 APPEND 续写，否则
/// create 截断新建。无写权限时服务器即时拒绝（permission_denied）
pub async fn open_remote_write(
    sftp: &SftpSession,
    path: &str,
    append: bool,
) -> Result<File, String> {
    let open = async {
        if append {
            sftp.open_with_flags(path, OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::APPEND)
                .await
        } else {
            sftp.create(path).await
        }
    };
    match tokio::time::timeout(SFTP_META_DEADLINE, open).await {
        Ok(r) => r.map_err(sftp_err),
        Err(_) => Err(format!("sftp open timeout (15s): {path}")),
    }
}


pub struct SftpController {
    sftp: Arc<SftpSession>,
    uploads: std::sync::Mutex<HashMap<String, UploadHandle>>,
    downloads: std::sync::Mutex<HashMap<String, JoinHandle<()>>>,
    /// 原生传输任务（get-*/put-*）：opId → 任务句柄（取消/会话关闭清理）
    transfers: std::sync::Mutex<HashMap<String, JoinHandle<()>>>,
    /// 远端写权限探针备忘：目录 → (时刻, 可写)；5s TTL，批量同目录预检去重
    probe_memo: std::sync::Mutex<HashMap<String, (Instant, bool)>>,
}

struct UploadHandle {
    tx: mpsc::UnboundedSender<UploadCmd>,
    done: JoinHandle<Result<(), String>>,
}

enum UploadCmd {
    Write(Vec<u8>),
    Finish,
}

impl SftpController {
    pub fn new(sftp: SftpSession) -> SftpController {
        SftpController {
            sftp: Arc::new(sftp),
            uploads: std::sync::Mutex::new(HashMap::new()),
            downloads: std::sync::Mutex::new(HashMap::new()),
            transfers: std::sync::Mutex::new(HashMap::new()),
            probe_memo: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 由共享的 SFTP 会话构建（连接池复用同一通道）
    pub fn from_shared(sftp: Arc<SftpSession>) -> SftpController {
        SftpController {
            sftp,
            uploads: std::sync::Mutex::new(HashMap::new()),
            downloads: std::sync::Mutex::new(HashMap::new()),
            transfers: std::sync::Mutex::new(HashMap::new()),
            probe_memo: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub async fn realpath(&self, path: &str) -> Result<String, String> {
        // ls 的首请求：半开死连接在源头快速失败（read_dir 大目录合法慢，不设限）
        match tokio::time::timeout(SFTP_META_DEADLINE, self.sftp.canonicalize(path)).await {
            Ok(r) => r.map_err(sftp_err),
            Err(_) => Err(format!("sftp realpath timeout (15s): {path}")),
        }
    }

    // ── 传输预检（preflight）────────────────────────────────────────────

    /// 远端侧预检：目标存在性/大小、剩余空间（statvfs 扩展）、写权限探针、建议重命名。
    /// `check_target`：上传方向（远端是写入目标）= true，做探针/建议名/剩余空间；
    /// 下载方向（远端是读取源）= false，只 stat。
    pub(crate) async fn remote_preflight(
        &self,
        remote_path: &str,
        check_target: bool,
    ) -> RemoteCheck {
        let mut check = RemoteCheck {
            exists: false,
            is_dir: false,
            size_b: None,
            free_b: None,
            writable: true,
            suggested_path: None,
        };
        match self.sftp.metadata(remote_path).await {
            Ok(attrs) => {
                check.exists = true;
                check.is_dir = attrs.is_dir();
                check.size_b = attrs.size;
            }
            Err(_) => {
                check.exists = false;
            }
        }
        if !check_target {
            return check;
        }
        let parent = dirname_remote(remote_path);
        // 父目录不存在：上传时由引擎 mkdir -p 建骨架，探针/剩余空间跳过（不误报不可写）
        if self.sftp.metadata(&parent).await.is_err() {
            return check;
        }
        // 剩余空间：statvfs@openssh.com 扩展，服务器不支持时留空（降级为 free_unknown 警告）
        if let Ok(Some(v)) = self.sftp.fs_info(&parent).await {
            let unit = if v.fragment_size > 0 { v.fragment_size } else { v.block_size };
            check.free_b = Some(v.blocks_avail.saturating_mul(unit));
        }
        check.writable = self.probe_writable(&parent).await;
        if check.exists && !check.is_dir {
            check.suggested_path = self.suggest_free_name(&parent, basename_remote(remote_path)).await;
        }
        check
    }

    /// 远端目录预检扫描（扫描远端目录大小的会话内包装）
    pub(crate) async fn scan_remote_size(
        &self,
        root: &str,
        budget: &ScanBudget,
    ) -> Result<(u64, u64, bool), String> {
        scan_remote_size(&self.sftp, root, budget).await
    }

    /// 远端目录写权限探针：创建临时文件 → 关闭 → 删除。
    /// 结果按目录 5s TTL 备忘，批量预检同目录只探一次。
    async fn probe_writable(&self, dir: &str) -> bool {
        if let Some((at, ok)) = self.probe_memo.lock().unwrap().get(dir) {
            if at.elapsed() < Duration::from_secs(5) {
                return *ok;
            }
        }
        let probe = join_remote(dir, &format!(".bg_probe_{}", uuid::Uuid::new_v4().simple()));
        let ok = match self.sftp.create(&probe).await {
            Ok(file) => {
                // 立即确认关闭，避免句柄滞留
                let _ = file.close().await;
                true
            }
            Err(_) => false,
        };
        // 用后必删（删除失败不影响结论——探针文件 0 字节且不可见）
        let _ = self.sftp.remove_file(&probe).await;
        self.probe_memo
            .lock()
            .unwrap()
            .insert(dir.to_string(), (Instant::now(), ok));
        ok
    }

    /// 生成不冲突的建议名：`name(1).ext`、`name(2).ext`…（列不出目录时返回 None）
    async fn suggest_free_name(&self, dir: &str, name: &str) -> Option<String> {
        let names: Option<HashMap<String, ()>> = match self.sftp.read_dir(dir).await {
            Ok(rd) => {
                let mut map = HashMap::new();
                for entry in rd {
                    map.insert(entry.file_name(), ());
                }
                Some(map)
            }
            Err(_) => None,
        };
        let names = names?;
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() && !e.is_empty() && !e.contains('/') => {
                (s.to_string(), format!(".{e}"))
            }
            _ => (name.to_string(), String::new()),
        };
        for n in 1..100u32 {
            let candidate = format!("{stem}({n}){ext}");
            if !names.contains_key(&candidate) {
                return Some(join_remote(dir, &candidate));
            }
        }
        None
    }


    pub async fn ls(&self, path: &str) -> Result<(String, Vec<SftpEntry>), String> {
        let resolved = self.realpath(path).await?;
        let dir = self.sftp.read_dir(&resolved).await.map_err(sftp_err)?;
        let mut entries: Vec<SftpEntry> = Vec::new();
        for entry in dir {
            let attrs = entry.metadata();
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let entry_type = if attrs.is_dir() {
                "dir"
            } else if attrs.is_symlink() {
                "symlink"
            } else {
                "file"
            };
            entries.push(SftpEntry {
                name,
                entry_type: entry_type.to_string(),
                size_b: attrs.size.unwrap_or(0),
                mtime_ms: attrs.mtime.map(|t| t as i64 * 1000).unwrap_or(0),
            });
        }
        entries.sort_by(|a, b| {
            if a.entry_type == b.entry_type {
                a.name.cmp(&b.name)
            } else if a.entry_type == "dir" {
                std::cmp::Ordering::Less
            } else if b.entry_type == "dir" {
                std::cmp::Ordering::Greater
            } else {
                a.name.cmp(&b.name)
            }
        });
        Ok((resolved, entries))
    }

    pub async fn mkdir(&self, path: &str) -> Result<(), String> {
        match tokio::time::timeout(SFTP_META_DEADLINE, self.sftp.create_dir(path)).await {
            Ok(r) => r.map_err(sftp_err),
            Err(_) => Err(format!("sftp mkdir timeout (15s): {path}")),
        }
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        match tokio::time::timeout(SFTP_META_DEADLINE, self.sftp.rename(from, to)).await {
            Ok(r) => r.map_err(sftp_err),
            Err(_) => Err(format!("sftp rename timeout (15s): {from} → {to}")),
        }
    }

    pub async fn rm(&self, path: &str, recursive: bool) -> Result<(), String> {
        // 递归 async fn 需要装箱（E0733）
        Box::pin(self.rm_inner(path, recursive)).await
    }

    async fn rm_inner(&self, path: &str, recursive: bool) -> Result<(), String> {
        if !recursive {
            return self.sftp.remove_file(path).await.map_err(sftp_err);
        }
        let meta = self.sftp.metadata(path).await.map_err(sftp_err)?;
        if !meta.is_dir() {
            return self.rm(path, false).await;
        }
        let dir = self.sftp.read_dir(path).await.map_err(sftp_err)?;
        for entry in dir {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            Box::pin(self.rm(&join_remote(path, &name), true)).await?;
        }
        self.sftp.remove_dir(path).await.map_err(sftp_err)
    }

    // ── 上传 ──────────────────────────────────────────────────────────────

    /// 打开上传流，返回 opId；create 在此等待完成（15s 超时）——无写权限/
    /// 死连接在 upload-open 即刻返回错误，前端无需推完全部块才在
    /// upload-close 发现失败。块经 upload_chunk 入队，upload_close 收尾。
    pub async fn upload_open(&self, path: &str) -> Result<String, String> {
        let file = open_remote_write(&self.sftp, path, false).await?;
        let op_id = uuid::Uuid::new_v4().to_string();
        let (tx, mut rx) = mpsc::unbounded_channel::<UploadCmd>();
        let done = tokio::spawn(async move {
            let mut file = file;
            let mut first_err: Option<String> = None;
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    UploadCmd::Write(chunk) => {
                        if first_err.is_none() {
                            if let Err(e) = file.write_all(&chunk).await {
                                first_err = Some(sftp_err(e));
                            }
                        }
                    }
                    UploadCmd::Finish => break,
                }
            }
            // 收尾总是执行：flush + shutdown（等待服务器确认关闭句柄，
            // 避免远端持有写 fd → ./xx.sh 报 Text file busy），保留首个错误
            if let Err(e) = file.flush().await {
                first_err = first_err.or_else(|| Some(sftp_err(e)));
            }
            if let Err(e) = file.shutdown().await {
                first_err = first_err.or_else(|| Some(sftp_err(e)));
            }
            first_err.map(Err).unwrap_or(Ok(()))
        });
        self.uploads
            .lock()
            .unwrap()
            .insert(op_id.clone(), UploadHandle { tx, done });
        Ok(op_id)
    }

    pub fn upload_chunk(&self, op_id: &str, chunk: Vec<u8>) {
        if let Some(h) = self.uploads.lock().unwrap().get(op_id) {
            let _ = h.tx.send(UploadCmd::Write(chunk));
        }
    }

    pub async fn upload_close(&self, op_id: &str) -> Result<(), String> {
        let handle = self.uploads.lock().unwrap().remove(op_id);
        let Some(h) = handle else { return Ok(()) };
        let _ = h.tx.send(UploadCmd::Finish);
        match h.done.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.to_string()),
        }
    }

    // ── 下载 ──────────────────────────────────────────────────────────────

    /// 打开下载流：stat 拿大小（15s 超时），后台任务按 64KB 块回送
    /// （0x11 数据帧 + 进度）。打开/读取失败立即回报 download-done(ok=false)，
    /// 不能静默结束——否则前端要等满超时才知道失败（无读权限曾表现为 20s 超时）
    pub async fn download_open(
        &self,
        session_id: &str,
        path: &str,
        out: &Outbound,
    ) -> Result<(String, u64), String> {
        let meta = stat_deadline(&self.sftp, path).await?;
        let size = meta.size.unwrap_or(0);
        let op_id = uuid::Uuid::new_v4().to_string();
        let sftp = self.sftp.clone();
        let path = path.to_string();
        let session_id = session_id.to_string();
        let out = out.clone();
        let op_id_task = op_id.clone();
        let task = tokio::spawn(async move {
            let mut file = match open_read_deadline(&sftp, &path).await {
                Ok(f) => f,
                Err(e) => {
                    send_download_done(&out, &session_id, &op_id_task, Some(e));
                    return;
                }
            };
            let mut buf = vec![0u8; CHUNK];
            let mut transferred: u64 = 0;
            let mut read_err: Option<String> = None;
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break,
                    // 读失败（连接断开等）如实回报，不能静默装成功让前端拿残缺数据
                    Err(e) => {
                        read_err = Some(sftp_err(e));
                        break;
                    }
                    Ok(n) => {
                        transferred += n as u64;
                        let payload = bg_core::protocol::encode_op_payload(&op_id_task, &buf[..n]);
                        out.send_binary(bg_core::protocol::encode_data_frame(
                            DF_SFTP_DOWNLOAD,
                            &session_id,
                            &payload,
                        ));
                        use bg_core::protocol::ServerFrame;
                        out.send_frame(&ServerFrame::SftpProgress {
                            s: session_id.clone(),
                            m: SftpProgressM {
                                op_id: op_id_task.clone(),
                                transferred,
                                total: size,
                                name: None,
                                file_done: None,
                                file_total: None,
                                speed_bps: None,
                                resumed_b: None,
                            },
                        });
                    }
                }
            }
            send_download_done(&out, &session_id, &op_id_task, read_err);
        });
        self.downloads.lock().unwrap().insert(op_id.clone(), task);
        Ok((op_id, size))
    }

    pub fn download_close(&self, op_id: &str) {
        if let Some(task) = self.downloads.lock().unwrap().remove(op_id) {
            task.abort();
        }
    }

    // ── 原生传输（Agent 直传，数据不经过 WebView）───────────────────────

    /// 下载远端文件到本地路径（断点续传）。立即返回 opId，进度经 sftp-progress、
    /// 结束经 transfer-done 帧回报。
    pub fn start_get_file(
        self: &Arc<Self>,
        session_id: &str,
        remote: &str,
        local: &str,
        out: &Outbound,
        fresh: bool,
    ) -> String {
        let op_id = uuid::Uuid::new_v4().to_string();
        let sftp = self.sftp.clone();
        let ctrl = self.clone();
        let state = Arc::new(XferState::new(out, session_id, &op_id));
        let remote = remote.to_string();
        let local = PathBuf::from(local);
        let sid = session_id.to_string();
        let task = tokio::spawn(async move {
            // 先取总大小（首个进度帧即可给出正确 total）
            match sftp.metadata(&remote).await {
                Ok(meta) => state.total.store(meta.size.unwrap_or(0), Ordering::Relaxed),
                Err(e) => {
                    send_transfer_done(&state, &sid, false, 0, Some(sftp_err(e)));
                    ctrl.finish_transfer(&state.op_id);
                    return;
                }
            }
            let res = xfer_download_one(sftp.clone(), remote.clone(), local, state.clone(), String::new(), fresh).await;
            match &res {
                Ok(bytes) => send_transfer_done(&state, &sid, true, *bytes, None),
                Err(e) => send_transfer_done(&state, &sid, false, 0, Some(e.clone())),
            }
            ctrl.finish_transfer(&state.op_id);
        });
        self.transfers.lock().unwrap().insert(op_id.clone(), task);
        op_id
    }

    /// 递归下载远端目录到本地目录（跳过符号链接；已存在且大小一致的文件自动跳过）
    pub fn start_get_dir(
        self: &Arc<Self>,
        session_id: &str,
        remote_dir: &str,
        local_dir: &str,
        out: &Outbound,
        fresh: bool,
    ) -> String {
        let op_id = uuid::Uuid::new_v4().to_string();
        let sftp = self.sftp.clone();
        let ctrl = self.clone();
        let state = Arc::new(XferState::new(out, session_id, &op_id));
        let remote_root = remote_dir.to_string();
        let local_root = PathBuf::from(local_dir);
        let sid = session_id.to_string();
        let task = tokio::spawn(async move {
            state.emit_scan("扫描远端目录…", true);
            let mut files = Vec::new();
            if let Err(e) = collect_remote(&sftp, &remote_root, &local_root, &mut files).await {
                send_transfer_done(&state, &sid, false, 0, Some(e));
                ctrl.finish_transfer(&state.op_id);
                return;
            }
            let total: u64 = files.iter().map(|f| f.2).sum();
            state.total.store(total, Ordering::Relaxed);
            state.file_total.store(files.len() as u64, Ordering::Relaxed);
            state.emit_scan("扫描完成", true);
            if files.is_empty() {
                send_transfer_done(&state, &sid, true, 0, None);
                ctrl.finish_transfer(&state.op_id);
                return;
            }
            // 建本地目录骨架（幂等；首个失败即中止）
            let mut seen = std::collections::HashSet::new();
            for (_, local, _) in &files {
                if let Some(parent) = local.parent() {
                    if !parent.as_os_str().is_empty() && seen.insert(parent.to_path_buf()) {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            send_transfer_done(
                                &state,
                                &sid,
                                false,
                                0,
                                Some(format!("创建本地目录失败：{e}")),
                            );
                            ctrl.finish_transfer(&state.op_id);
                            return;
                        }
                    }
                }
            }
            let failures = run_downloads(sftp.clone(), files, state.clone(), fresh).await;
            let ok = failures.is_empty();
            send_transfer_done(
                &state,
                &sid,
                ok,
                state.transferred.load(Ordering::Relaxed),
                failures.first().cloned(),
            );
            ctrl.finish_transfer(&state.op_id);
        });
        self.transfers.lock().unwrap().insert(op_id.clone(), task);
        op_id
    }

    /// 上传本地文件到远端路径（远端已有部分时用 APPEND 续传）
    pub fn start_put_file(
        self: &Arc<Self>,
        session_id: &str,
        local: &str,
        remote: &str,
        out: &Outbound,
        fresh: bool,
    ) -> String {
        let op_id = uuid::Uuid::new_v4().to_string();
        let sftp = self.sftp.clone();
        let ctrl = self.clone();
        let state = Arc::new(XferState::new(out, session_id, &op_id));
        let local = PathBuf::from(local);
        let remote = remote.to_string();
        let sid = session_id.to_string();
        let task = tokio::spawn(async move {
            match tokio::fs::metadata(&local).await {
                Ok(m) => state.total.store(m.len(), Ordering::Relaxed),
                Err(e) => {
                    send_transfer_done(&state, &sid, false, 0, Some(format!("读取本地文件失败：{e}")));
                    ctrl.finish_transfer(&state.op_id);
                    return;
                }
            }
            let res = xfer_upload_one(sftp.clone(), local, remote.clone(), state.clone(), String::new(), fresh).await;
            match &res {
                Ok(bytes) => send_transfer_done(&state, &sid, true, *bytes, None),
                Err(e) => send_transfer_done(&state, &sid, false, 0, Some(e.clone())),
            }
            ctrl.finish_transfer(&state.op_id);
        });
        self.transfers.lock().unwrap().insert(op_id.clone(), task);
        op_id
    }

    /// 递归上传本地目录到远端目录（跳过符号链接；远端已有部分时按文件续传）
    pub fn start_put_dir(
        self: &Arc<Self>,
        session_id: &str,
        local_dir: &str,
        remote_dir: &str,
        out: &Outbound,
        fresh: bool,
    ) -> String {
        let op_id = uuid::Uuid::new_v4().to_string();
        let sftp = self.sftp.clone();
        let ctrl = self.clone();
        let state = Arc::new(XferState::new(out, session_id, &op_id));
        let local_root = PathBuf::from(local_dir);
        let remote_root = remote_dir.to_string();
        let sid = session_id.to_string();
        let task = tokio::spawn(async move {
            state.emit_scan("扫描本地目录…", true);
            let mut files = Vec::new();
            if let Err(e) = collect_local(&local_root, &remote_root, &mut files) {
                send_transfer_done(&state, &sid, false, 0, Some(e));
                ctrl.finish_transfer(&state.op_id);
                return;
            }
            let total: u64 = files.iter().map(|f| f.2).sum();
            state.total.store(total, Ordering::Relaxed);
            state.file_total.store(files.len() as u64, Ordering::Relaxed);
            state.emit_scan("扫描完成", true);
            if files.is_empty() {
                send_transfer_done(&state, &sid, true, 0, None);
                ctrl.finish_transfer(&state.op_id);
                return;
            }
            // 远端目录骨架（已存在/无权限的失败留给后续文件写暴露）
            let mut seen = std::collections::HashSet::new();
            for (remote_file, _, _) in &files {
                if let Some((parent, _)) = remote_file.rsplit_once('/') {
                    if !parent.is_empty() && seen.insert(parent.to_string()) {
                        let _ = mkdir_remote_p(&sftp, parent).await;
                    }
                }
            }
            let failures = run_uploads(sftp.clone(), files, state.clone(), fresh).await;
            let ok = failures.is_empty();
            send_transfer_done(
                &state,
                &sid,
                ok,
                state.transferred.load(Ordering::Relaxed),
                failures.first().cloned(),
            );
            ctrl.finish_transfer(&state.op_id);
        });
        self.transfers.lock().unwrap().insert(op_id.clone(), task);
        op_id
    }

    /// 取消传输任务（保留已下载的 .part 以便续传）
    pub fn transfer_cancel(&self, op_id: &str) -> bool {
        if let Some(task) = self.transfers.lock().unwrap().remove(op_id) {
            task.abort();
            true
        } else {
            false
        }
    }

    fn finish_transfer(&self, op_id: &str) {
        self.transfers.lock().unwrap().remove(op_id);
    }

    /// 任务收尾（上传/下载/传输句柄清理），不动底层 SFTP 会话：
    /// 会话关闭时调用；底层 sftp 由连接注册表统一关闭（多会话共享同一通道）
    pub async fn close_ops(&self) {
        // 先收集句柄再 await（MutexGuard 不能跨 await 持有）
        let uploads: Vec<UploadHandle> = {
            self.uploads.lock().unwrap().drain().map(|(_, h)| h).collect()
        };
        for h in uploads {
            let _ = h.tx.send(UploadCmd::Finish);
            let _ = h.done.await;
        }
        let downloads: Vec<JoinHandle<()>> = {
            self.downloads.lock().unwrap().drain().map(|(_, t)| t).collect()
        };
        for task in downloads {
            task.abort();
        }
        let transfers: Vec<JoinHandle<()>> = {
            self.transfers.lock().unwrap().drain().map(|(_, t)| t).collect()
        };
        for task in transfers {
            task.abort();
        }
    }
}

/// POSIX 远程路径拼接（SFTP 目标多为 Linux）
pub fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

// ── 原生传输引擎 ───────────────────────────────────────────────────────
// 设计参考成熟下载器（aria2 / FileZilla）：
//   * 断点续传：下载写 <file>.part 部分文件，重传时从本地已有字节处续读远端；
//     上传检测远端已有大小，用 SFTP APPEND 语义续写，跳过本地已传前缀。
//   * 目录任务：递归扫描建清单 → 3 文件并发（SFTP 请求在同一连接上多路复用，
//     高延迟链路下多文件流水线可显著提速）。
//   * 大块传输：256KB 缓冲（russh-sftp 按服务器 max_read_len/max_write_len
//     自动切分，写入侧自带并发 ack 流水线）。
//   * 进度：200ms 节流 + EMA 平滑速度；结束发 transfer-done。

const XFER_CHUNK: usize = 256 * 1024;
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(200);
const DIR_CONCURRENCY: usize = 3;

/// 任务级共享进度状态（目录任务的多个并发子文件共同累加）
pub struct XferState {
    out: Outbound,
    sid: String,
    op_id: String,
    total: AtomicU64,
    transferred: AtomicU64,
    file_done: AtomicU64,
    file_total: AtomicU64,
    /// 节流 + 速度估计（多任务并发发送互斥）
    inner: std::sync::Mutex<XferInner>,
}

struct XferInner {
    last_emit: Instant,
    speed_last_bytes: u64,
    speed_last_at: Instant,
    speed_ema_bps: f64,
}

impl XferState {
    fn new(out: &Outbound, sid: &str, op_id: &str) -> XferState {
        XferState {
            out: out.clone(),
            sid: sid.to_string(),
            op_id: op_id.to_string(),
            total: AtomicU64::new(0),
            transferred: AtomicU64::new(0),
            file_done: AtomicU64::new(0),
            file_total: AtomicU64::new(0),
            inner: std::sync::Mutex::new(XferInner {
                last_emit: Instant::now() - PROGRESS_MIN_INTERVAL,
                speed_last_bytes: 0,
                speed_last_at: Instant::now(),
                speed_ema_bps: 0.0,
            }),
        }
    }

    /// AI 路径构造：无 WS 出口的空进度状态（emit 帧全部静默丢弃）
    pub fn noop() -> XferState {
        let (tx, _rx) = mpsc::unbounded_channel();
        XferState::new(&Outbound { tx }, "ai", "noop")
    }

    /// 发送进度帧（节流；force = 文件完成/任务边界等关键节点）
    fn emit(&self, name: &str, force: bool) {
        let mut g = self.inner.lock().unwrap();
        if !force && g.last_emit.elapsed() < PROGRESS_MIN_INTERVAL {
            return;
        }
        g.last_emit = Instant::now();
        let transferred = self.transferred.load(Ordering::Relaxed);
        let now = Instant::now();
        let dt = now.duration_since(g.speed_last_at).as_secs_f64();
        if dt >= 0.5 {
            let bps = transferred.saturating_sub(g.speed_last_bytes) as f64 / dt;
            g.speed_ema_bps = if g.speed_ema_bps <= 0.0 {
                bps
            } else {
                0.6 * g.speed_ema_bps + 0.4 * bps
            };
            g.speed_last_bytes = transferred;
            g.speed_last_at = now;
        }
        let speed = g.speed_ema_bps as u64;
        drop(g);
        let file_total = self.file_total.load(Ordering::Relaxed);
        self.out.send_frame(&ServerFrame::SftpProgress {
            s: self.sid.clone(),
            m: SftpProgressM {
                op_id: self.op_id.clone(),
                transferred,
                total: self.total.load(Ordering::Relaxed),
                name: if name.is_empty() { None } else { Some(name.to_string()) },
                file_done: if file_total > 0 {
                    Some(self.file_done.load(Ordering::Relaxed))
                } else {
                    None
                },
                file_total: if file_total > 0 { Some(file_total) } else { None },
                speed_bps: Some(speed),
                resumed_b: None,
            },
        });
    }

    /// 目录扫描阶段进度（无字节量，报告已发现的文件数）
    fn emit_scan(&self, label: &str, force: bool) {
        let file_total = self.file_total.load(Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap();
        if !force && g.last_emit.elapsed() < PROGRESS_MIN_INTERVAL {
            return;
        }
        g.last_emit = Instant::now();
        drop(g);
        self.out.send_frame(&ServerFrame::SftpProgress {
            s: self.sid.clone(),
            m: SftpProgressM {
                op_id: self.op_id.clone(),
                transferred: self.transferred.load(Ordering::Relaxed),
                total: self.total.load(Ordering::Relaxed),
                name: Some(label.to_string()),
                file_done: Some(self.file_done.load(Ordering::Relaxed)),
                file_total: if file_total > 0 { Some(file_total) } else { None },
                speed_bps: None,
                resumed_b: None,
            },
        });
    }
}

/// WS 流式下载结束帧（message = Some 时 ok=false 并附错误分类）
fn send_download_done(out: &Outbound, session_id: &str, op_id: &str, message: Option<String>) {
    let ok = message.is_none();
    let error_kind = message.as_deref().and_then(classify_sftp_error);
    out.send_frame(&ServerFrame::SftpResult {
        s: session_id.to_string(),
        m: SftpResultM {
            op: "download-done".into(),
            ok,
            rid: None,
            op_id: Some(op_id.to_string()),
            resolved_path: None,
            entries: None,
            size_b: None,
            host: None,
            message,
            error_kind,
            preflight: None,
            dl_base: None,
        },
    });
}

/// 传输结束帧（ok 时 sizeB = 传输字节数；失败时附带错误分类）
fn send_transfer_done(state: &XferState, sid: &str, ok: bool, bytes: u64, message: Option<String>) {
    let error_kind = message.as_deref().and_then(classify_sftp_error);
    state.out.send_frame(&ServerFrame::SftpResult {
        s: sid.to_string(),
        m: SftpResultM {
            op: "transfer-done".into(),
            ok,
            rid: None,
            op_id: Some(state.op_id.clone()),
            resolved_path: None,
            entries: None,
            size_b: Some(bytes),
            host: None,
            message,
            error_kind,
            preflight: None,
            dl_base: None,
        },
    });
}

fn part_path(local: &Path) -> PathBuf {
    let mut s = local.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

/// 单文件下载（断点续传；fresh = 覆盖语义：忽略已存在目标与 .part 断点，全新重传）
pub async fn xfer_download_one(
    sftp: Arc<SftpSession>,
    remote: String,
    local: PathBuf,
    state: Arc<XferState>,
    display: String,
    fresh: bool,
) -> Result<u64, String> {
    let state = &*state;
    let display = display.as_str();
    let local = local.as_path();
    let remote = remote.as_str();
    let sftp = &*sftp;
    let meta = sftp.metadata(remote).await.map_err(sftp_err)?;
    let size = meta.size.unwrap_or(0);
    // 目标已完整存在 → 跳过（目录重下幂等；fresh 覆盖时强制重传）
    if !fresh {
        if let Ok(m) = tokio::fs::metadata(local).await {
            if m.len() == size {
                state.transferred.fetch_add(size, Ordering::Relaxed);
                state.file_done.fetch_add(1, Ordering::Relaxed);
                state.emit(display, true);
                return Ok(size);
            }
        }
    }
    let part = part_path(local);
    let part_len = if fresh {
        None // 覆盖：忽略旧断点（resume 0 → set_len(0) 截断）
    } else {
        tokio::fs::metadata(&part).await.ok().map(|m| m.len())
    };
    let resume_from = download_resume_from(size, part_len);
    let mut remote_file = sftp.open(remote).await.map_err(sftp_err)?;
    if resume_from > 0 {
        remote_file
            .seek(std::io::SeekFrom::Start(resume_from))
            .await
            .map_err(sftp_err)?;
    }
    // 打开本地 .part：续传 → append 追加；全新 → truncate 截断式一步打开
    // （替代先 append 再 set_len 的两步，缩小被锁窗口；杀软/同步盘瞬时锁定时重试）
    let mut local_file = if resume_from > 0 {
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part)
            .await
            .map_err(|e| format!("打开本地文件失败：{e}"))?
    } else {
        let mut last_err = None;
        let mut opened = None;
        for attempt in 0..3u32 {
            match tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&part)
                .await
            {
                Ok(f) => {
                    opened = Some(f);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                }
            }
        }
        opened.ok_or_else(|| format!("打开本地文件失败：{}", last_err.unwrap()))?
    };
    state.transferred.fetch_add(resume_from, Ordering::Relaxed);
    state.emit(display, true);
    let mut buf = vec![0u8; XFER_CHUNK];
    loop {
        let n = match remote_file.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(sftp_err(e)),
        };
        local_file
            .write_all(&buf[..n])
            .await
            .map_err(|e| format!("写本地文件失败：{e}"))?;
        state.transferred.fetch_add(n as u64, Ordering::Relaxed);
        state.emit(display, false);
    }
    local_file
        .flush()
        .await
        .map_err(|e| format!("写本地文件失败：{e}"))?;
    drop(local_file);
    tokio::fs::rename(&part, local)
        .await
        .map_err(|e| format!("落盘失败：{e}"))?;
    state.file_done.fetch_add(1, Ordering::Relaxed);
    state.emit(display, true);
    Ok(size - resume_from)
}

/// 单文件上传（断点续传；fresh = 覆盖语义：无视远端已有内容，create 截断全量重传）
pub async fn xfer_upload_one(
    sftp: Arc<SftpSession>,
    local: PathBuf,
    remote: String,
    state: Arc<XferState>,
    display: String,
    fresh: bool,
) -> Result<u64, String> {
    let state = &*state;
    let display = display.as_str();
    let local = local.as_path();
    let remote = remote.as_str();
    let sftp = &*sftp;
    let size = tokio::fs::metadata(local)
        .await
        .map_err(|e| format!("读取本地文件失败：{e}"))?
        .len();
    let remote_size: Option<u64> = if fresh {
        None // 覆盖：跳过续传判定，走 create 截断重建
    } else {
        match sftp.metadata(remote).await {
            Ok(m) => m.size,
            Err(_) => None,
        }
    };
    let (mut file, offset) = match remote_size {
        Some(rs) if rs == size => {
            state.transferred.fetch_add(size, Ordering::Relaxed);
            state.file_done.fetch_add(1, Ordering::Relaxed);
            state.emit(display, true);
            return Ok(size);
        }
        // 远端小于本地：续传（APPEND 语义，服务器忽略写偏移）
        _ if upload_resume_from(size, remote_size).is_some() => (
            sftp.open_with_flags(remote, OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::APPEND)
                .await
                .map_err(sftp_err)?,
            remote_size.unwrap_or(0),
        ),
        // 无远端文件或远端反超（本地已变化）：全量重传
        _ => (sftp.create(remote).await.map_err(sftp_err)?, 0),
    };
    let mut local_file = tokio::fs::File::open(local)
        .await
        .map_err(|e| format!("打开本地文件失败：{e}"))?;
    if offset > 0 {
        local_file
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| format!("定位本地文件失败：{e}"))?;
    }
    state.transferred.fetch_add(offset, Ordering::Relaxed);
    state.emit(display, true);
    let mut buf = vec![0u8; XFER_CHUNK];
    loop {
        let n = match local_file.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(format!("读本地文件失败：{e}")),
        };
        file.write_all(&buf[..n]).await.map_err(sftp_err)?;
        state.transferred.fetch_add(n as u64, Ordering::Relaxed);
        state.emit(display, false);
    }
    file.flush().await.map_err(sftp_err)?;
    file.shutdown().await.map_err(sftp_err)?;
    state.file_done.fetch_add(1, Ordering::Relaxed);
    state.emit(display, true);
    Ok(size - offset)
}

/// 递归收集远端清单：Vec<(远端路径, 本地路径, 大小)>（跳过符号链接防环）
async fn collect_remote(
    sftp: &SftpSession,
    remote_dir: &str,
    local_dir: &Path,
    files: &mut Vec<(String, PathBuf, u64)>,
) -> Result<(), String> {
    let dir = sftp.read_dir(remote_dir).await.map_err(sftp_err)?;
    let mut entries: Vec<_> = dir
        .into_iter()
        .map(|e| (e.file_name(), e.metadata()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, attrs) in entries {
        if name == "." || name == ".." {
            continue;
        }
        let remote_path = join_remote(remote_dir, &name);
        let local_path = local_dir.join(&name);
        if attrs.is_dir() {
            Box::pin(collect_remote(sftp, &remote_path, &local_path, files)).await?;
        } else if attrs.is_symlink() {
            continue;
        } else {
            files.push((remote_path, local_path, attrs.size.unwrap_or(0)));
        }
    }
    Ok(())
}

/// 递归收集本地清单：Vec<(远端路径, 本地路径, 大小)>
fn collect_local(
    local_dir: &Path,
    remote_root: &str,
    files: &mut Vec<(String, PathBuf, u64)>,
) -> Result<(), String> {
    let rd = std::fs::read_dir(local_dir).map_err(|e| format!("读取本地目录失败：{e}"))?;
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let local_path = entry.path();
        let remote_path = join_remote(remote_root, &name);
        let md = entry
            .metadata()
            .map_err(|e| format!("读取本地元数据失败 {name}: {e}"))?;
        if md.is_dir() {
            collect_local(&local_path, &remote_path, files)?;
        } else if md.file_type().is_symlink() {
            continue;
        } else {
            files.push((remote_path, local_path, md.len()));
        }
    }
    Ok(())
}

/// 远端 mkdir -p（逐级；"已存在"等失败静默——真正的问题会由文件写暴露）
async fn mkdir_remote_p(sftp: &SftpSession, remote_dir: &str) -> Result<(), String> {
    if remote_dir.is_empty() || remote_dir == "/" {
        return Ok(());
    }
    if let Some((parent, _)) = remote_dir.rsplit_once('/') {
        if !parent.is_empty() {
            Box::pin(mkdir_remote_p(sftp, parent)).await?;
        }
    }
    match sftp.create_dir(remote_dir).await {
        Ok(()) => Ok(()),
        // 已存在 → 视为成功
        Err(_) => Ok(()),
    }
}

/// 目录下载并发执行器：DIR_CONCURRENCY 文件流水线，返回失败原因列表
async fn run_downloads(
    sftp: Arc<SftpSession>,
    files: Vec<(String, PathBuf, u64)>,
    state: Arc<XferState>,
    fresh: bool,
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut stream = futures_util::stream::iter(files.into_iter().map(|(remote, local, _)| {
        let sftp = sftp.clone();
        let state = state.clone();
        let display = remote.clone();
        async move {
            let r = xfer_download_one(sftp, remote, local, state, display.clone(), fresh).await;
            (display, r)
        }
    }))
    .buffer_unordered(DIR_CONCURRENCY);
    while let Some((name, res)) = stream.next().await {
        if let Err(e) = res {
            failures.push(format!("{name}: {e}"));
        }
    }
    failures
}

/// 目录上传并发执行器（同 run_downloads，方向相反）
async fn run_uploads(
    sftp: Arc<SftpSession>,
    files: Vec<(String, PathBuf, u64)>,
    state: Arc<XferState>,
    fresh: bool,
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut stream = futures_util::stream::iter(files.into_iter().map(|(remote, local, _)| {
        let sftp = sftp.clone();
        let state = state.clone();
        let display = remote.clone();
        async move {
            let r = xfer_upload_one(sftp, local, remote, state, display.clone(), fresh).await;
            (display, r)
        }
    }))
    .buffer_unordered(DIR_CONCURRENCY);
    while let Some((name, res)) = stream.next().await {
        if let Err(e) = res {
            failures.push(format!("{name}: {e}"));
        }
    }
    failures
}

/// 下载断点决策：根据远端大小与本地 .part 状态得出续传起点
fn download_resume_from(remote_size: u64, part_len: Option<u64>) -> u64 {
    match part_len {
        // 有部分文件且未超过远端大小 → 续传；超过（远端已变化）→ 重传
        Some(n) if n > 0 && n <= remote_size => n,
        _ => 0,
    }
}

/// 上传续传决策：Some(offset) = 从本地 offset 起以 APPEND 续写；None = 全量重传
fn upload_resume_from(local_size: u64, remote_size: Option<u64>) -> Option<u64> {
    match remote_size {
        // 已完整 → 由调用方跳过；此处仅判续传区间
        Some(rs) if rs > 0 && rs < local_size => Some(rs),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_covers_common_cases() {
        assert_eq!(
            classify_sftp_error("Permission denied"),
            Some("permission_denied".to_string())
        );
        assert_eq!(
            classify_sftp_error("open /x: failure: No space left on device"),
            Some("no_space".to_string())
        );
        assert_eq!(classify_sftp_error("Quota exceeded"), Some("quota".to_string()));
        assert_eq!(classify_sftp_error("File exists"), Some("file_exists".to_string()));
        assert_eq!(
            classify_sftp_error("Text file busy"),
            Some("text_file_busy".to_string())
        );
        assert_eq!(
            classify_sftp_error("connection reset by peer"),
            Some("network".to_string())
        );
        assert_eq!(
            classify_sftp_error("No such file"),
            Some("not_found".to_string())
        );
        assert_eq!(
            classify_sftp_error("打开本地文件失败：拒绝访问。"),
            Some("local_io".to_string())
        );
        assert_eq!(
            classify_sftp_error("重置部分文件失败：拒绝访问。 (os error 5)"),
            Some("local_io".to_string())
        );
        assert_eq!(classify_sftp_error("whatever"), None);
    }

    #[test]
    fn remote_path_helpers() {
        assert_eq!(dirname_remote("/a/b/c.sh"), "/a/b");
        assert_eq!(dirname_remote("/c.sh"), "/");
        assert_eq!(dirname_remote("/a/b/"), "/a");
        assert_eq!(basename_remote("/a/b/c.sh"), "c.sh");
        assert_eq!(basename_remote("c.sh"), "c.sh");
    }

    #[test]
    fn scan_local_size_budget_and_truncation() {
        let dir = std::env::temp_dir().join("bg_scan_test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.bin"), vec![0u8; 1024]).unwrap();
        std::fs::write(dir.join("b.bin"), vec![0u8; 2048]).unwrap();
        // 足够预算 → 全量
        let (total, files, truncated) =
            scan_local_size(&dir, &ScanBudget { max_files: 100, deadline: Instant::now() + Duration::from_secs(5) })
                .unwrap();
        assert_eq!((total, files, truncated), (1024 + 2048, 2, false));
        // max_files = 1 → 截断
        let (t2, f2, trunc2) =
            scan_local_size(&dir, &ScanBudget { max_files: 1, deadline: Instant::now() + Duration::from_secs(5) })
                .unwrap();
        assert!(trunc2);
        assert_eq!(f2, 1);
        assert!(t2 > 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn part_path_appends_suffix() {
        let p = part_path(Path::new(if cfg!(windows) {
            r"C:\tmp\archive.tar.gz"
        } else {
            "/tmp/archive.tar.gz"
        }));
        assert!(p.to_string_lossy().ends_with(".tar.gz.part"));
    }

    #[test]
    fn resume_decisions() {
        // 下载：部分文件在区间内续传，越界（远端变化）重传
        assert_eq!(download_resume_from(1000, Some(400)), 400);
        assert_eq!(download_resume_from(1000, Some(1000)), 1000);
        assert_eq!(download_resume_from(1000, Some(1200)), 0);
        assert_eq!(download_resume_from(1000, Some(0)), 0);
        assert_eq!(download_resume_from(1000, None), 0);
        // 上传：远端部分存在续传，否则重传
        assert_eq!(upload_resume_from(1000, Some(300)), Some(300));
        assert_eq!(upload_resume_from(1000, Some(1000)), None);
        assert_eq!(upload_resume_from(1000, Some(1500)), None);
        assert_eq!(upload_resume_from(1000, None), None);
    }

    #[test]
    fn join_remote_variants() {
        assert_eq!(join_remote("/a", "b"), "/a/b");
        assert_eq!(join_remote("/a/", "b"), "/a/b");
        assert_eq!(join_remote("/", "b"), "/b");
    }
}
