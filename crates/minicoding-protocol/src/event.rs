//! Event DTO（从 `core::Event` 映射，携带 `seq: u64`）。
//!
//! 前端通过 SSE 订阅事件流，记录最后接收的 `seq`，断线重连时用 `seq` 恢复。
//! `EventDto` 是 `core::Event` 的序列化友好表示，所有变体共享 `seq` 字段。
//!
//! NDJSON 协议（T-M8-4）扩展：除 `core::Event` 映射的变体外，新增
//! `SessionsListed`/`SessionRetrieved`/`CommandError` 三个 NDJSON 专用变体——
//! 这些变体不对应 `core::Event`，由 NDJSON 适配器在响应非流式命令时直接构造，
//! 不经过 `From<&Event>` 转换。

use minicoding_core::model::{Message, SessionMeta, StopReason, Task, ToolCallId, ToolResult};
use minicoding_core::policy::{Decision, PermissionMode, Risk};
use minicoding_core::runtime::Event;
use serde::{Deserialize, Serialize};

/// 事件 DTO（携带 `seq`，用于 SSE cursor 恢复）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDto {
    /// 单调递增序列号（每会话独立计数）。
    pub seq: u64,
    /// 事件种类。
    #[serde(flatten)]
    pub kind: EventKind,
}

/// 事件种类（与 `core::Event` 一一映射，但序列化为 tagged enum）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// 流式 token 增量。
    Token { text: String },
    /// 一条消息已追加（落盘 + 入上下文后）。
    MessageAppended { message: Message },
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
    SessionCreated { id: String },
    /// 权限已询问。
    PermissionRequested {
        id: String,
        tool: String,
        summary: String,
        risk: Risk,
    },
    /// 权限已 resolved。
    PermissionResolved { id: String, decision: Decision },
    /// 权限模式切换。
    PermissionModeChanged {
        from: PermissionMode,
        to: PermissionMode,
    },
    /// 任务更新。
    TaskUpdated { task: Task },
    /// NDJSON 专用：`ListSessions` 命令响应（不对应 `core::Event`，由 NDJSON 适配器构造）。
    SessionsListed { sessions: Vec<SessionMeta> },
    /// NDJSON 专用：`GetSession` 命令响应（不对应 `core::Event`，由 NDJSON 适配器构造）。
    SessionRetrieved {
        session_id: String,
        messages: Vec<Message>,
    },
    /// NDJSON 专用：命令错误响应（不对应 `core::Event`，由 NDJSON 适配器构造）。
    ///
    /// 用于：JSON 解析失败、会话不存在、命令不支持等。`seq` 字段为 0（非流式事件）。
    CommandError { message: String },
}

impl From<&Event> for EventKind {
    fn from(e: &Event) -> Self {
        match e {
            Event::Token(text) => Self::Token { text: text.clone() },
            Event::MessageAppended(msg) => Self::MessageAppended {
                message: msg.clone(),
            },
            Event::TurnStreamingStarted => Self::TurnStreamingStarted,
            Event::TurnEnd { stop_reason } => Self::TurnEnd {
                stop_reason: stop_reason.clone(),
            },
            Event::ToolCallStarted { call_id, tool } => Self::ToolCallStarted {
                call_id: call_id.clone(),
                tool: tool.clone(),
            },
            Event::ToolCallFinished { call_id, result } => Self::ToolCallFinished {
                call_id: call_id.clone(),
                result: result.clone(),
            },
            Event::SessionCreated { id } => Self::SessionCreated { id: id.clone() },
            Event::PermissionRequested {
                id,
                tool,
                summary,
                risk,
            } => Self::PermissionRequested {
                id: id.clone(),
                tool: tool.clone(),
                summary: summary.clone(),
                risk: *risk,
            },
            Event::PermissionResolved { id, decision } => Self::PermissionResolved {
                id: id.clone(),
                decision: decision.clone(),
            },
            Event::PermissionModeChanged { from, to } => Self::PermissionModeChanged {
                from: *from,
                to: *to,
            },
            Event::TaskUpdated { task } => Self::TaskUpdated { task: task.clone() },
        }
    }
}

impl EventDto {
    /// 从 `core::Event` 构造 DTO（附带 `seq`）。
    #[must_use]
    pub fn from_event(seq: u64, event: &Event) -> Self {
        Self {
            seq,
            kind: EventKind::from(event),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn token_event_roundtrip() {
        let dto = EventDto {
            seq: 42,
            kind: EventKind::Token {
                text: "hello".into(),
            },
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"seq\":42"));
        assert!(json.contains("\"type\":\"token\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 42);
    }

    #[test]
    fn from_core_event() {
        let event = Event::Token("world".into());
        let dto = EventDto::from_event(7, &event);
        match dto.kind {
            EventKind::Token { text } => assert_eq!(text, "world"),
            _ => panic!("wrong variant"),
        }
    }
}
