//! `ContextManager` trait（见 `api.md` §3.7）。
//!
//! M1 简化：Runtime 直接持有消息列表，M3 完整实现压缩管道。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容。

use crate::config::RuntimeConfig;
use crate::model::{Message, RuntimeError};
use crate::provider::{BoxFuture, ChatRequest};
use crate::tool::ToolRegistry;

/// 上下文快照（用于压缩前后状态保留）。
#[derive(Debug, Clone, Default)]
pub struct ContextSnapshot {
    pub messages: Vec<Message>,
    pub token_count: usize,
}

/// 上下文管理器 trait（`dyn` 兼容）。
pub trait ContextManager: Send + Sync {
    fn append(&self, msg: Message) -> BoxFuture<'_, ()>;
    fn build_chat_request(
        &self,
        tools: &ToolRegistry,
        config: &RuntimeConfig,
    ) -> BoxFuture<'_, Result<ChatRequest, RuntimeError>>;
    fn snapshot(&self) -> BoxFuture<'_, ContextSnapshot>;
    fn restore(&self, snap: ContextSnapshot) -> BoxFuture<'_, ()>;
    fn token_count(&self) -> usize;
    fn message_count(&self) -> usize;
}
