//! bg-server：WS 服务泛化 crate。
//! 各壳（desktop/ssh/sql/serial）组装自己的 Dispatcher 路由表，共用同一条
//! /ws 帧通道（Origin 白名单 / 写任务隔离 / 数据帧解包）。

mod server;

pub use server::{ConnHandler, Dispatcher, WsServer};
