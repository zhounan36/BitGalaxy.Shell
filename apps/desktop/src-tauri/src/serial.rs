//! 串口会话通道（P1：控制台透传，方案见 .trae/documents/串口通信方案.md）。
//! kind="serial" 的会话不走 SSH 连接注册表：本机 COM 口经 tokio-serial 打开，
//! 读半部循环 → DF_STDOUT 数据帧，stdin 队列 → 专职写任务（与 shell 会话同构）。
//! 热插拔：读错误（拔线/端口消失）→ 会话以 "serial-lost" 关闭。
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio_serial::SerialStream;

use crate::protocol::{SerialOptions, SerialPortM};

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

/// 串口写通道（读半部由会话 reader 任务接管转发为 DF_STDOUT）。
/// resize 对串口无意义（无 PTY 概念），会话层直接忽略。
pub struct SerialChannel {
    write: Mutex<tokio::io::WriteHalf<SerialStream>>,
}

impl SerialChannel {
    /// 打开串口，返回写通道与读半部。
    /// 打开失败给出分类提示（占用/不存在等），经 fail_open 呈现给用户。
    pub fn open(
        opts: &SerialOptions,
    ) -> Result<(Arc<SerialChannel>, tokio::io::ReadHalf<SerialStream>), String> {
        let builder = serialport::new(&opts.port, opts.baud_rate)
            .data_bits(map_data_bits(opts.data_bits)?)
            .parity(map_parity(opts.parity.as_deref())?)
            .stop_bits(map_stop_bits(opts.stop_bits)?)
            .flow_control(map_flow_control(opts.flow_control.as_deref())?);
        let stream = SerialStream::open(&builder).map_err(|e| classify_open_error(&e, &opts.port))?;
        let (read, write) = tokio::io::split(stream);
        Ok((
            Arc::new(SerialChannel {
                write: Mutex::new(write),
            }),
            read,
        ))
    }

    /// 串口写入（5s 写超时，与 PTY write_input 一致：端口异常时快速失败，
    /// 避免会话 stdin 写任务无限挂起堆积）
    pub async fn write_input(&self, data: &[u8]) -> Result<(), String> {
        let mut w = self.write.lock().await;
        match tokio::time::timeout(Duration::from_secs(5), w.write_all(data)).await {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err("写入超时（串口可能已被移除）".into()),
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
