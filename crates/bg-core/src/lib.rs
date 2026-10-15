//! bg-core：四 EXE 架构的共享基座 crate。
//! 所有壳（desktop/ssh/sql/serial）与上层 crate（bg-ssh/bg-ai/bg-sql/bg-server）
//! 共用的协议、存储、策略与基础工具都在这里，不依赖任何上层模块。

pub mod ai_base;
pub mod db;
pub mod disk;
pub mod error;
pub mod policy;
pub mod protocol;
pub mod stores;
pub mod wire;
