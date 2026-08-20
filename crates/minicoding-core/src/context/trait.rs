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
    /// 会话 id 提示（M-07，R-02）：Runtime 构造时调用，供实现记录当前会话，
    /// 用于压缩审计（`AuditKind::Compress`）等需要会话标识的内部记录。
    ///
    /// 默认 no-op——不要求所有实现支持（如测试用 `ContextManager` 无需记录）。
    fn set_session_hint(&self, _id: &str) {}
}
