//! bg-serial：串口 crate。
//! 本机 COM 口枚举（list_ports）、双工会话通道（SerialChannel）、
//! 会话生命周期管理（SerialSessionManager：open/input/serial-ctl/close）。

pub mod manager;
pub mod serial;

pub use manager::SerialSessionManager;
