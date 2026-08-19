//! 数据模型：消息、工具、会话、错误类型。
//!
//! 所有类型可序列化（`serde`），跨 crate 共享。定义在 core 保证依赖方向干净。

pub mod error;
pub mod message;
pub mod session;
pub mod subagent;
pub mod task;
pub mod tool;

pub use error::{
    ExtensionError, JournalError, LlmError, McpError, MemoryError, PolicyError, PromptError,
    RuntimeError, StorageError, ToolError,
};
pub use message::repair_dangling_tool_calls;
pub use message::{ContentBlock, Message, MessageMeta, MessageSource, Role};
pub use session::{
    Attachment, ContextHint, Session, SessionId, SessionMeta, StopReason, TurnOutcome, UserInput,
};
pub use subagent::{
    Isolation, MergeStrategy, SubagentResult, SubagentSpec, SubagentType, Thoroughness,
    WorktreeSpec,
};
pub use task::{Task, TaskStatus};
pub use tool::{
    SideEffect, ToolCall, ToolCallId, ToolContent, ToolResult, ToolResultMeta, ToolSchema,
};
