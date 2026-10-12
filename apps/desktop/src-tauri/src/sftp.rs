//! SFTP 文件管理（kind=sftp 会话）：目录操作 + 流式上传/下载（64KB 块）。
//! 上传按 opId 建有序命令队列（保持块顺序、不阻塞 WS 读循环）。
use std::collections::HashMap;
use std::sync::Arc;

use russh_sftp::client::fs::File;
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::protocol::{SftpEntry, DF_SFTP_DOWNLOAD};
use crate::wire::Outbound;

const CHUNK: usize = 64 * 1024;

fn sftp_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub struct SftpController {
    sftp: Arc<SftpSession>,
    uploads: std::sync::Mutex<HashMap<String, UploadHandle>>,
    downloads: std::sync::Mutex<HashMap<String, JoinHandle<()>>>,
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
        }
    }

    /// 由共享的 SFTP 会话构建（连接池复用同一通道）
    pub fn from_shared(sftp: Arc<SftpSession>) -> SftpController {
        SftpController {
            sftp,
            uploads: std::sync::Mutex::new(HashMap::new()),
            downloads: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub async fn realpath(&self, path: &str) -> Result<String, String> {
        self.sftp.canonicalize(path).await.map_err(sftp_err)
    }

    pub async fn ls(&self, path: &str) -> Result<(String, Vec<SftpEntry>), String> {
        let resolved = self.realpath(path).await?;
        let dir = self.sftp.read_dir(&resolved).await.map_err(sftp_err)?;
        let mut entries: Vec<SftpEntry> = Vec::new();
        {
            use futures_util::stream::StreamExt;
            let mut dir = Box::pin(dir);
            while let Some(entry) = dir.next().await {
                let meta = entry.metadata();
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let entry_type = if meta.is_dir() {
                    "dir"
                } else if meta.is_symlink() {
                    "symlink"
                } else if meta.is_file() {
                    "file"
                } else {
                    "other"
                };
                entries.push(SftpEntry {
                    name,
                    entry_type: entry_type.to_string(),
                    size_b: meta.size().unwrap_or(0),
                    mtime_ms: meta.mtime().map(|t| t * 1000).unwrap_or(0),
                });
            }
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
        self.sftp.create_dir(path).await.map_err(sftp_err)
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.sftp.rename(from, to).await.map_err(sftp_err)
    }

    pub async fn rm(&self, path: &str, recursive: bool) -> Result<(), String> {
        if !recursive {
            return self.sftp.remove_file(path).await.map_err(sftp_err);
        }
        let meta = self.sftp.metadata(path).await.map_err(sftp_err)?;
        if !meta.is_dir() {
            return self.rm(path, false).await;
        }
        let dir = self.sftp.read_dir(path).await.map_err(sftp_err)?;
        {
            use futures_util::stream::StreamExt;
            let mut dir = Box::pin(dir);
            while let Some(entry) = dir.next().await {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                self.rm(&join_remote(path, &name), true).await?;
            }
        }
        self.sftp.remove_dir(path).await.map_err(sftp_err)
    }

    // ── 上传 ──────────────────────────────────────────────────────────────

    /// 打开上传流，返回 opId；块经 upload_chunk 入队，upload_close 收尾
    pub fn upload_open(&self, path: &str) -> String {
        let op_id = uuid::Uuid::new_v4().to_string();
        let sftp = self.sftp.clone();
        let path = path.to_string();
        let (tx, mut rx) = mpsc::unbounded_channel::<UploadCmd>();
        let done = tokio::spawn(async move {
            let mut file: File = match sftp.create(&path).await {
                Ok(f) => f,
                Err(e) => return Err(sftp_err(e)),
            };
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
            if first_err.is_none() {
                if let Err(e) = file.flush().await {
                    first_err = Some(sftp_err(e));
                }
                if let Err(e) = file.shutdown().await {
                    first_err = Some(sftp_err(e));
                }
            }
            first_err.map(Err).unwrap_or(Ok(()))
        });
        self.uploads
            .lock()
            .unwrap()
            .insert(op_id.clone(), UploadHandle { tx, done });
        op_id
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

    /// 打开下载流：stat 拿大小，后台任务按 64KB 块回送（0x11 数据帧 + 进度）
    pub async fn download_open(
        &self,
        session_id: &str,
        path: &str,
        out: &Outbound,
    ) -> Result<(String, u64), String> {
        let meta = self.sftp.metadata(path).await.map_err(sftp_err)?;
        let size = meta.size().unwrap_or(0);
        let op_id = uuid::Uuid::new_v4().to_string();
        let sftp = self.sftp.clone();
        let path = path.to_string();
        let session_id = session_id.to_string();
        let out = out.clone();
        let op_id_task = op_id.clone();
        let task = tokio::spawn(async move {
            let Ok(mut file) = sftp.open(&path).await else {
                return;
            };
            let mut buf = vec![0u8; CHUNK];
            let mut transferred: u64 = 0;
            loop {
                match file.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        transferred += n as u64;
                        let payload = crate::protocol::encode_op_payload(&op_id_task, &buf[..n]);
                        out.send_binary(crate::protocol::encode_data_frame(
                            DF_SFTP_DOWNLOAD,
                            &session_id,
                            &payload,
                        ));
                        use crate::protocol::ServerFrame;
                        out.send_frame(&ServerFrame::SftpProgress {
                            s: session_id.clone(),
                            m: crate::protocol::SftpProgressM {
                                op_id: op_id_task.clone(),
                                transferred,
                                total: size,
                            },
                        });
                    }
                }
            }
            use crate::protocol::{ServerFrame, SftpResultM};
            out.send_frame(&ServerFrame::SftpResult {
                s: session_id,
                m: SftpResultM {
                    op: "download-done".into(),
                    ok: true,
                    rid: None,
                    op_id: Some(op_id_task),
                    resolved_path: None,
                    entries: None,
                    size_b: None,
                    message: None,
                },
            });
        });
        self.downloads.lock().unwrap().insert(op_id.clone(), task);
        Ok((op_id, size))
    }

    pub fn download_close(&self, op_id: &str) {
        if let Some(task) = self.downloads.lock().unwrap().remove(op_id) {
            task.abort();
        }
    }

    pub async fn close(&self) {
        for (_, h) in self.uploads.lock().unwrap().drain() {
            let _ = h.tx.send(UploadCmd::Finish);
            let _ = h.done.await;
        }
        for (_, t) in self.downloads.lock().unwrap().drain() {
            t.abort();
        }
        let _ = self.sftp.close().await;
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
