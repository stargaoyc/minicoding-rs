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

    /// 用 provider 返回的真实 `Usage.input_tokens` 校准本地 token 估算
    /// （2026-08-23 审查遗留#2）：本地 BPE/近似分词与 provider 实际计费口径
    /// 存在漂移，长会话累积显著。默认 no-op；实现方以指数平滑吸收校准值，
    /// 避免单次异常值抖动。
    fn calibrate(&self, _actual_input_tokens: usize) {}

    /// 强制压缩（PT4-3，2026-08-28 R8 审查）：真实 400 上下文超长
    /// （`LlmError::ContextLength`）时由 Runtime 主动触发一次压缩后重试——
    /// 此时上下文可能未超**本地**阈值（模型真实窗口 < 配置窗口，如 `DeepSeek` 64K
    /// 配成 128K），`build_chat_request` 的阈值判定不会压缩，必须强制。
    ///
    /// 默认 no-op（兼容只读/测试实现）；`ContextManagerImpl` 实现为走完整
    /// 4 级压缩管道 + 熔断/降级链（C-29）。
    ///
    /// # Errors
    /// 压缩失败或熔断触发时返回 `RuntimeError`。
    fn force_compress(&self) -> BoxFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async { Ok(()) })
    }
}
