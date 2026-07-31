//! 事件总线（仅通知无回复，见 `design.md` §11）。
//!
//! `EventBus` 基于 `tokio::sync::broadcast`：发布者克隆事件，订阅者各自消费。
//! 权限交互不走总线（点对点 `PermissionPrompter`，见 `policy::trait`）。

use crate::model::{Message, SessionId, StopReason, ToolCallId, ToolResult};
use crate::policy::{Decision, Risk};
use tokio::sync::broadcast;

/// 运行时事件（向前端广播，仅通知无回复通道）。
#[derive(Debug, Clone)]
pub enum Event {
    /// 流式 token 增量。
    Token(String),
    /// 一条消息已追加（落盘 + 入上下文后）。
    MessageAppended(Message),
    /// 流式开始。
    TurnStreamingStarted,
    /// 一轮结束。
    TurnEnd { stop_reason: StopReason },
    /// 工具调用开始。
    ToolCallStarted { call_id: ToolCallId, tool: String },
    /// 工具调用完成。
    ToolCallFinished {
        call_id: ToolCallId,
        result: ToolResult,
    },
    /// 会话已创建。
    SessionCreated { id: SessionId },
    /// 权限已询问（通知类，仅展示/审计，无回复通道，见 `design.md` §9.2）。
    PermissionRequested {
        id: String,
        tool: String,
        summary: String,
        risk: Risk,
    },
    /// 权限已 resolved（带最终决策，供 UI 关闭弹窗与审计，见 `design.md` §9.2）。
    PermissionResolved { id: String, decision: Decision },
}

/// 事件总线（broadcast channel）。
///
/// 容量 256：token 事件高频，过小会丢消息；过大浪费内存。
/// 订阅者消费慢时丢弃最旧事件（对 token 流可接受）。
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    /// 创建事件总线。
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(256)
    }

    /// 指定容量创建。
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self { tx }
    }

    /// 发布事件（无订阅者时静默丢弃）。
    pub fn emit(&self, event: Event) {
        // send 失败仅因无订阅者，不视为错误
        let _ = self.tx.send(event);
    }

    /// 订阅事件流。
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("subscribers", &self.tx.receiver_count())
            .finish()
    }
}
