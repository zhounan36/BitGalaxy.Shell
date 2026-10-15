//! bg-ai：AI 引擎 crate。
//! - ai.rs：LLM 对话循环（OpenAI 兼容）+ 桌宠审批 UI（PetApprover）
//! - toolcore.rs：工具核心（命令分类、15 个工具、审批抽象、审计）
//! - mcp.rs：MCP server stdio 通道（--mcp-stdio 子模式）
//! AiHandler 进程级单例语义由宿主壳保证（见 server 组装层）。

pub mod ai;
pub mod mcp;
pub mod toolcore;
