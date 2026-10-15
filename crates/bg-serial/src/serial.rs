//! 串口会话通道（P1 控制台透传 + P2 调试控制，方案见 .trae/documents/串口通信方案.md）。
//! kind="serial" 的会话不走 SSH 连接注册表：本机 COM 口经 tokio-serial 打开。
//!
//! P2 起改为「单 select! 双工任务独占 SerialStream」模型：split 后拿不回
//! DTR/RTS/Break 等控制方法（且 Windows COM 句柄单开），因此整个流由一个
//! 任务独占，交错处理 读（→ DF_STDOUT 回调）与 Cmd 通道（写入/控制命令）。
//! 热插拔：读错误（拔线/端口消失）→ on_end("serial-lost") 回调。
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};
use tokio_serial::{SerialPort, SerialStream};

use bg_core::protocol::{SerialOptions, SerialPortM, SerialSignalsM};

/// 枚举本机串口（serial-ports 帧负载）
pub fn list_ports() -> Result<Vec<SerialPortM>, String> {
    let infos = serialport::available_ports().map_err(|e| e.to_string())?;
    Ok(infos
        .into_iter()
        .map(|p| {
            let (kind, description, serial_number) = match &p.port_type {
                serialport::SerialPortType::UsbPort(u) => (
                    "usb",
                    Some(
                        [u.manufacturer.as_deref(), u.product.as_deref()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    u.serial_number.clone(),
                ),
                serialport::SerialPortType::BluetoothPort => ("bluetooth", None, None),
                serialport::SerialPortType::PciPort => ("pci", None, None),
                _ => ("unknown", None, None),
            };
            SerialPortM {
                name: p.port_name,
                kind: kind.to_string(),
                description: description.filter(|d| !d.is_empty()),
                serial_number,
            }
        })
        .collect())
}

/// 双工任务命令（由 SerialChannel 各方法投递）
enum Cmd {
    Data(Vec<u8>, oneshot::Sender<Result<(), String>>),
    Dtr(bool, oneshot::Sender<Result<(), String>>),
    Rts(bool, oneshot::Sender<Result<(), String>>),
    /// 拉起 Break：set_break 立即生效并回执，持续 ms 后由内部定时器自动 clear
    Break(u32, oneshot::Sender<Result<(), String>>),
    /// Break 持续时间到（内部定时器投递，无需回执）
    BreakEnd,
    Baud(u32, oneshot::Sender<Result<(), String>>),
    Signals(oneshot::Sender<Result<SerialSignalsM, String>>),
}

/// 串口通道句柄：所有读写/控制经 Cmd 通道送达独占 SerialStream 的双工任务。
pub struct SerialChannel {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
}

impl SerialChannel {
    /// 打开串口并启动双工任务。
    /// `on_data`：读到的字节（DF_STDOUT 推送）；`on_end`：读循环结束
    /// （"serial-closed" 端口正常关闭 / "serial-lost" 拔线或端口消失）。
    /// 返回通道句柄 + 双工任务 JoinHandle（会话关闭时 abort，释放端口句柄）。
    pub fn open(
        opts: &SerialOptions,
        on_data: impl Fn(&[u8]) + Send + 'static,
        on_end: impl Fn(&'static str) + Send + 'static,
    ) -> Result<(Arc<SerialChannel>, tokio::task::JoinHandle<()>), String> {
        let builder = serialport::new(&opts.port, opts.baud_rate)
            .data_bits(map_data_bits(opts.data_bits)?)
            .parity(map_parity(opts.parity.as_deref())?)
            .stop_bits(map_stop_bits(opts.stop_bits)?)
            .flow_control(map_flow_control(opts.flow_control.as_deref())?);
        let mut stream = SerialStream::open(&builder).map_err(|e| classify_open_error(&e, &opts.port))?;

        // 打开时 DTR/RTS 预设（Arduino 复位/ESP32 下载模式依赖；缺省全拉高）
        let _ = stream.write_data_terminal_ready(opts.dtr);
        let _ = stream.write_request_to_send(opts.rts);

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
        // 任务自持一份 sender 供 Break 定时器回投 BreakEnd：
        // 因此 cmd_rx.recv() 不会因句柄全 drop 而返回 None，任务退出统一由
        // 会话层 abort（handle_closed → serial_reader.abort()）负责
        let break_tx = cmd_tx.clone();
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                tokio::select! {
                    read = stream.read(&mut buf) => {
                        match read {
                            Ok(n) if n > 0 => on_data(&buf[..n]),
                            Ok(_) => {
                                // 串口读到 0 字节（端口被关闭）视同结束
                                on_end("serial-closed");
                                return;
                            }
                            Err(_) => {
                                on_end("serial-lost");
                                return;
                            }
                        }
                    }
                    cmd = cmd_rx.recv() => {
                        let Some(cmd) = cmd else { return };
                        handle_cmd(&mut stream, cmd, &break_tx).await;
                    }
                }
            }
        });
        Ok((Arc::new(SerialChannel { cmd_tx }), task))
    }

    /// 串口写入（5s 写超时，与 PTY write_input 一致：端口异常时快速失败，
    /// 避免会话 stdin 写任务无限挂起堆积）
    pub async fn write_input(&self, data: &[u8]) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Data(data.to_vec(), tx))
            .map_err(|_| "串口已关闭".to_string())?;
        wait_ack(rx).await
    }

    /// DTR 电平控制（Arduino 自动复位、设备复位脚控制）
    pub async fn set_dtr(&self, level: bool) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Dtr(level, tx))
            .map_err(|_| "串口已关闭".to_string())?;
        wait_ack(rx).await
    }

    /// RTS 电平控制
    pub async fn set_rts(&self, level: bool) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Rts(level, tx))
            .map_err(|_| "串口已关闭".to_string())?;
        wait_ack(rx).await
    }

    /// 发送 Break（持续 ms 后自动恢复；serialport 无时长版 write_break，
    /// set_break + 定时 clear_break 配对，回执表示 Break 已拉起）
    pub async fn send_break(&self, ms: u32) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Break(ms, tx))
            .map_err(|_| "串口已关闭".to_string())?;
        wait_ack(rx).await
    }

    /// 运行时改波特率（对未知设备扫波特率用）
    pub async fn set_baud(&self, baud: u32) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Baud(baud, tx))
            .map_err(|_| "串口已关闭".to_string())?;
        wait_ack(rx).await
    }

    /// 查询输入信号线状态（CD/CTS/DSR/RI）
    pub async fn signals(&self) -> Result<SerialSignalsM, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Signals(tx))
            .map_err(|_| "串口已关闭".to_string())?;
        match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("串口已关闭".into()),
            Err(_) => Err("信号查询超时".into()),
        }
    }
}

/// 等待双工任务回执（5s 超时统一口径：端口异常时快速失败不挂起）
async fn wait_ack(rx: oneshot::Receiver<Result<(), String>>) -> Result<(), String> {
    match tokio::time::timeout(Duration::from_secs(5), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("串口已关闭".into()),
        Err(_) => Err("串口操作超时".into()),
    }
}

/// 双工任务内执行控制命令（独占 SerialStream，可安全 await 写入）
async fn handle_cmd(
    stream: &mut SerialStream,
    cmd: Cmd,
    break_tx: &mpsc::UnboundedSender<Cmd>,
) {
    match cmd {
        Cmd::Data(data, ack) => {
            use tokio::io::AsyncWriteExt;
            // 写超时 5s：端口异常时快速失败，不让写阻塞后续命令
            let r = match tokio::time::timeout(Duration::from_secs(5), stream.write_all(&data)).await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(_) => Err("写入超时（串口可能已被移除）".into()),
            };
            let _ = ack.send(r);
        }
        Cmd::Dtr(level, ack) => {
            let _ = ack.send(stream.write_data_terminal_ready(level).map_err(|e| e.to_string()));
        }
        Cmd::Rts(level, ack) => {
            let _ = ack.send(stream.write_request_to_send(level).map_err(|e| e.to_string()));
        }
        Cmd::Break(ms, ack) => {
            let r = stream.set_break().map_err(|e| e.to_string());
            let ok = r.is_ok();
            let _ = ack.send(r);
            if ok {
                // 持续 ms 后自动 clear_break（定时器投递 BreakEnd 回双工任务）
                let tx = break_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(ms as u64)).await;
                    let _ = tx.send(Cmd::BreakEnd);
                });
            }
        }
        Cmd::BreakEnd => {
            let _ = stream.clear_break();
        }
        Cmd::Baud(baud, ack) => {
            let _ = ack.send(stream.set_baud_rate(baud).map_err(|e| e.to_string()));
        }
        Cmd::Signals(ack) => {
            let r = (|| -> Result<SerialSignalsM, String> {
                Ok(SerialSignalsM {
                    cd: stream.read_carrier_detect().map_err(|e| e.to_string())?,
                    cts: stream.read_clear_to_send().map_err(|e| e.to_string())?,
                    dsr: stream.read_data_set_ready().map_err(|e| e.to_string())?,
                    ri: stream.read_ring_indicator().map_err(|e| e.to_string())?,
                })
            })();
            let _ = ack.send(r);
        }
    }
}

fn map_data_bits(v: u8) -> Result<serialport::DataBits, String> {
    match v {
        5 => Ok(serialport::DataBits::Five),
        6 => Ok(serialport::DataBits::Six),
        7 => Ok(serialport::DataBits::Seven),
        8 => Ok(serialport::DataBits::Eight),
        _ => Err(format!("无效的数据位数: {v}（支持 5-8）")),
    }
}

fn map_parity(v: Option<&str>) -> Result<serialport::Parity, String> {
    // serialport 4.x 仅支持 none/odd/even（Mark/Space 已移除）
    match v.unwrap_or("none") {
        "none" => Ok(serialport::Parity::None),
        "odd" => Ok(serialport::Parity::Odd),
        "even" => Ok(serialport::Parity::Even),
        other => Err(format!("无效的校验位: {other}（none/odd/even）")),
    }
}

fn map_stop_bits(v: Option<u8>) -> Result<serialport::StopBits, String> {
    match v.unwrap_or(1) {
        1 => Ok(serialport::StopBits::One),
        2 => Ok(serialport::StopBits::Two),
        other => Err(format!("无效的停止位: {other}（支持 1/2）")),
    }
}

fn map_flow_control(v: Option<&str>) -> Result<serialport::FlowControl, String> {
    match v.unwrap_or("none") {
        "none" => Ok(serialport::FlowControl::None),
        "software" => Ok(serialport::FlowControl::Software),
        "hardware" => Ok(serialport::FlowControl::Hardware),
        other => Err(format!("无效的流控: {other}（none/software/hardware）")),
    }
}

/// 打开失败的友好提示（占用/不存在是最常见的两类用户可自解问题）
fn classify_open_error(e: &serialport::Error, port: &str) -> String {
    match &e.kind {
        serialport::ErrorKind::Io(io) if *io == std::io::ErrorKind::PermissionDenied => {
            format!("串口 {port} 被占用（其他程序正在使用），请关闭占用程序后重试")
        }
        serialport::ErrorKind::Io(io) if *io == std::io::ErrorKind::NotFound => {
            format!("串口 {port} 不存在（设备已拔出或名称有误），可刷新端口列表")
        }
        serialport::ErrorKind::NoDevice => {
            format!("串口 {port} 不存在（设备已拔出或名称有误），可刷新端口列表")
        }
        _ => format!("打开串口 {port} 失败: {e}"),
    }
}
