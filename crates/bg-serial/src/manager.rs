//! 串口会话管理器（从 bg-ssh session.rs 剥离，2026-10 四 EXE 拆分 P3）：
//! kind="serial" 的会话生命周期（open / input / serial-ctl / close）。
//! 不进连接注册表（本机 COM 口无共享复用需求）、无 TOFU/密码重试弹窗。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use bg_core::protocol::{self, SerialCtlM, SerialCtlResultM, SerialOptions, ServerFrame, SessionOpenedM, DF_STDOUT};
use bg_core::stores::Stores;
use bg_core::wire::Outbound;

use crate::serial::SerialChannel;

/// 一条串口会话
struct SerialEntry {
    channel: Arc<SerialChannel>,
    /// stdin 专用写入任务 sender：按序消费队列写串口
    stdin_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// 双工 IO 任务（读循环）：关闭会话时 abort，SerialStream drop 即释放 COM 句柄
    io_task: JoinHandle<()>,
}

/// 串口会话管理器：克隆即共享（内部 Mutex）
#[derive(Clone)]
pub struct SerialSessionManager {
    sessions: Arc<Mutex<HashMap<String, SerialEntry>>>,
    out: Outbound,
    stores: Stores,
}

impl SerialSessionManager {
    pub fn new(stores: Stores, out: Outbound) -> Self {
        SerialSessionManager { sessions: Arc::new(Mutex::new(HashMap::new())), out, stores }
    }

    fn send_control(&self, frame: ServerFrame) {
        self.out.send_frame(&frame);
    }

    fn fail_open(&self, session_id: &str, reason: &str) {
        self.send_control(ServerFrame::error(Some(session_id), "open-failed", reason.to_string()));
        self.handle_closed(session_id, &format!("open-failed: {reason}"));
    }

    /// 打开串口会话（kind="serial"）：本机 COM 口透传到终端面板。
    /// 双工 IO 任务独占 SerialStream（读 → DF_STDOUT；stdin 队列经 Cmd 写）；
    /// resize 无意义（会话层忽略）。
    pub async fn open(&self, session_id: &str, serial_opts: Option<SerialOptions>) {
        {
            let sessions = self.sessions.lock().unwrap();
            if sessions.contains_key(session_id) {
                return;
            }
        }
        let Some(serial_opts) = serial_opts else {
            self.fail_open(session_id, "缺少串口参数（serial.port）");
            return;
        };
        let (dtr_preset, rts_preset) = (serial_opts.dtr, serial_opts.rts);

        // 双工任务：读字节 → DF_STDOUT（按 sessionId 推回，与 shell reader 同路径）；
        // 读 Err = 拔线/端口消失（热插拔）→ serial-lost 关闭会话
        let opened = {
            let this = self.clone();
            let sid = session_id.to_string();
            let this2 = self.clone();
            let sid2 = session_id.to_string();
            SerialChannel::open(
                &serial_opts,
                move |data| {
                    this.out
                        .send_binary(protocol::encode_data_frame(DF_STDOUT, &sid, data));
                },
                move |reason| {
                    this2.handle_closed(&sid2, reason);
                },
            )
        };
        let (channel, io_task) = match opened {
            Ok(x) => x,
            Err(e) => {
                self.fail_open(session_id, &e);
                return;
            }
        };

        // stdin 专用写入任务：按序消费队列写串口（写失败即退出，sender drop 自然结束）
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        {
            let ch = channel.clone();
            tokio::spawn(async move {
                while let Some(data) = stdin_rx.recv().await {
                    if ch.write_input(&data).await.is_err() {
                        break;
                    }
                }
            });
        }

        let port = serial_opts.port.clone();
        let baud = serial_opts.baud_rate;
        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.get(session_id).is_some() {
                // 建连期间被用户关闭（或 reader 已抢先触发 serial-lost）：
                // 双工任务随 abort/drop 释放端口，不再发送 SessionOpened
                io_task.abort();
                return;
            }
            sessions.insert(session_id.to_string(), SerialEntry { channel, stdin_tx, io_task });
        }

        self.send_control(ServerFrame::SessionOpened {
            s: session_id.to_string(),
            m: SessionOpenedM {
                host_id: "serial".into(),
                kind: "serial".into(),
                dtr: Some(dtr_preset),
                rts: Some(rts_preset),
            },
        });
        self.stores.audit.append(
            "session.opened",
            serde_json::json!({
                "sessionId": session_id,
                "kind": "serial",
                "target": format!("{port} @ {baud}"),
            }),
        );
    }

    /// serial-ctl：串口控制（DTR/RTS/Break/改波特率/信号查询）。
    /// 控制失败不杀会话（读循环的 serial-lost 兜底）。
    pub async fn serial_ctl(&self, session_id: &str, m: SerialCtlM) {
        let fail = |op: &str, msg: String| {
            self.send_control(ServerFrame::SerialCtlResult {
                s: session_id.to_string(),
                m: SerialCtlResultM {
                    ok: false,
                    op: op.to_string(),
                    message: Some(msg),
                    signals: None,
                },
            });
        };
        let lookup = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).map(|e| e.channel.clone())
        };
        let chan = match lookup {
            Some(chan) => chan,
            None => {
                fail(&m.op, "会话不存在或已关闭".to_string());
                return;
            }
        };

        let result = match m.op.as_str() {
            "dtr" => chan.set_dtr(m.level.unwrap_or(true)).await.map(|_| None),
            "rts" => chan.set_rts(m.level.unwrap_or(true)).await.map(|_| None),
            "break" => chan.send_break(m.ms.unwrap_or(250)).await.map(|_| None),
            "set-baud" => match m.baud_rate {
                Some(b) => chan.set_baud(b).await.map(|_| None),
                None => Err("缺少 baudRate".to_string()),
            },
            "signals" => chan.signals().await.map(Some),
            other => Err(format!("未知 serial-ctl 操作: {other}")),
        };
        let (ok, message, signals) = match result {
            Ok(sig) => (true, None, sig),
            Err(e) => (false, Some(e), None),
        };
        self.send_control(ServerFrame::SerialCtlResult {
            s: session_id.to_string(),
            m: SerialCtlResultM {
                ok,
                op: m.op,
                message,
                signals,
            },
        });
    }

    /// 终端输入（DF_STDIN 数据帧）：入队即返回，stdin 任务异步写入
    pub fn input(&self, session_id: &str, data: &[u8]) {
        let tx = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).map(|e| e.stdin_tx.clone())
        };
        if let Some(tx) = tx {
            let _ = tx.send(data.to_vec());
        }
    }

    pub fn close(&self, session_id: &str, _reason: &str) {
        self.handle_closed(session_id, "user");
    }

    pub fn close_all(&self, reason: &str) {
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.close(&id, reason);
        }
    }

    fn handle_closed(&self, session_id: &str, _reason: &str) {
        let removed = self.sessions.lock().unwrap().remove(session_id);
        let Some(entry) = removed else { return };
        // 串口 IO 双工任务：abort 后任务 future（内含整个 SerialStream）被 drop，
        // SerialStream 的 NamedPipeClient 持有 COM 句柄 → drop 即 CloseHandle，
        // 端口句柄在下一个调度 tick 释放（无需显式 close）
        entry.io_task.abort();
        // stdin_tx 随 entry drop 断链，写入任务自然结束
    }
}
