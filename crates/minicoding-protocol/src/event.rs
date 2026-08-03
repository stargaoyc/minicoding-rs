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
use minicoding_core::storage::PersistedEvent;
use serde::{Deserialize, Serialize};

/// 事件 DTO（携带 `seq`，用于 SSE cursor 恢复）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct EventDto {
    /// 单调递增序列号（每会话独立计数）。
    ///
    /// TS 端用 `number`（实际值远小于 2^53，无需 `bigint`）。
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub seq: u64,
    /// 事件种类。
    #[cfg_attr(feature = "ts", ts(flatten))]
    #[serde(flatten)]
    pub kind: EventKind,
}

/// 事件种类（与 `core::Event` 一一映射，但序列化为 tagged enum）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
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
    /// 配置文件变更（S-22 热更新）。
    ConfigChanged,
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
            Event::ConfigChanged => Self::ConfigChanged,
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

impl EventKind {
    /// 从 `PersistedEvent` 构造 `EventKind`（SSE durable recovery 用，见
    /// `design.md` §25.5）。
    ///
    /// 与 `From<&Event>` 的区别：`PersistedEvent` 是 `Event` 的持久化子集
    /// （仅状态变更事件），不含瞬态事件（`Token`/`TurnStreamingStarted` 等）。
    /// SSE handler 在内存 ring buffer evict 但 `after_seq <= durable_seq` 时，
    /// 调 `EventStore::load_after` 获取 `PersistedEvent` 列表，用此方法转为
    /// `EventKind` JSON 推送给客户端。
    ///
    /// 客户端应容忍瞬态事件缺失（如 `Token` 增量已被 `MessageAppended` 捕获）。
    #[must_use]
    pub fn from_persisted(p: &PersistedEvent) -> Self {
        match p {
            PersistedEvent::SessionCreated { id, .. } => Self::SessionCreated { id: id.clone() },
            PersistedEvent::MessageAppended { message } => Self::MessageAppended {
                message: message.clone(),
            },
            PersistedEvent::PermissionResolved { id, decision } => Self::PermissionResolved {
                id: id.clone(),
                decision: decision.clone(),
            },
            PersistedEvent::PermissionModeChanged { from, to } => Self::PermissionModeChanged {
                from: *from,
                to: *to,
            },
            PersistedEvent::TaskUpdated { task } => Self::TaskUpdated { task: task.clone() },
            PersistedEvent::TurnEnd { stop_reason } => Self::TurnEnd {
                stop_reason: stop_reason.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::model::{Message, Task, ToolResult};
    use minicoding_core::storage::PersistedEvent;
    use time::OffsetDateTime;

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

    // ---- From<&Event> 全变体覆盖 ----

    #[test]
    fn from_event_message_appended() {
        let msg = Message::user_text("hi");
        let dto = EventDto::from_event(1, &Event::MessageAppended(msg));
        match dto.kind {
            EventKind::MessageAppended { message } => {
                assert!(!message.content.is_empty());
            }
            _ => panic!("expected MessageAppended"),
        }
    }

    #[test]
    fn from_event_turn_streaming_started() {
        let dto = EventDto::from_event(2, &Event::TurnStreamingStarted);
        assert!(matches!(dto.kind, EventKind::TurnStreamingStarted));
    }

    #[test]
    fn from_event_turn_end() {
        let dto = EventDto::from_event(
            3,
            &Event::TurnEnd {
                stop_reason: StopReason::MaxTokens,
            },
        );
        match dto.kind {
            EventKind::TurnEnd { stop_reason } => {
                assert_eq!(stop_reason, StopReason::MaxTokens);
            }
            _ => panic!("expected TurnEnd"),
        }
    }

    #[test]
    fn from_event_tool_call_started() {
        let dto = EventDto::from_event(
            4,
            &Event::ToolCallStarted {
                call_id: "call-1".to_string(),
                tool: "fs.read".to_string(),
            },
        );
        match dto.kind {
            EventKind::ToolCallStarted { call_id, tool } => {
                assert_eq!(call_id, "call-1");
                assert_eq!(tool, "fs.read");
            }
            _ => panic!("expected ToolCallStarted"),
        }
    }

    #[test]
    fn from_event_tool_call_finished() {
        let result = ToolResult::ok_text("done");
        let dto = EventDto::from_event(
            5,
            &Event::ToolCallFinished {
                call_id: "call-2".to_string(),
                result: result.clone(),
            },
        );
        match dto.kind {
            EventKind::ToolCallFinished { call_id, result: r } => {
                assert_eq!(call_id, "call-2");
                assert!(!r.is_error);
            }
            _ => panic!("expected ToolCallFinished"),
        }
    }

    #[test]
    fn from_event_session_created() {
        let dto = EventDto::from_event(
            6,
            &Event::SessionCreated {
                id: "sess-abc".to_string(),
            },
        );
        match dto.kind {
            EventKind::SessionCreated { id } => assert_eq!(id, "sess-abc"),
            _ => panic!("expected SessionCreated"),
        }
    }

    #[test]
    fn from_event_permission_requested() {
        let dto = EventDto::from_event(
            7,
            &Event::PermissionRequested {
                id: "perm-1".to_string(),
                tool: "fs.write".to_string(),
                summary: "write file".to_string(),
                risk: Risk::High,
            },
        );
        match dto.kind {
            EventKind::PermissionRequested {
                id,
                tool,
                summary,
                risk,
            } => {
                assert_eq!(id, "perm-1");
                assert_eq!(tool, "fs.write");
                assert_eq!(summary, "write file");
                assert_eq!(risk, Risk::High);
            }
            _ => panic!("expected PermissionRequested"),
        }
    }

    #[test]
    fn from_event_permission_resolved() {
        let dto = EventDto::from_event(
            8,
            &Event::PermissionResolved {
                id: "perm-1".to_string(),
                decision: Decision::Deny("nope".to_string()),
            },
        );
        match dto.kind {
            EventKind::PermissionResolved { id, decision } => {
                assert_eq!(id, "perm-1");
                assert!(matches!(decision, Decision::Deny(_)));
            }
            _ => panic!("expected PermissionResolved"),
        }
    }

    #[test]
    fn from_event_permission_mode_changed() {
        let dto = EventDto::from_event(
            9,
            &Event::PermissionModeChanged {
                from: PermissionMode::Default,
                to: PermissionMode::AcceptEdits,
            },
        );
        match dto.kind {
            EventKind::PermissionModeChanged { from, to } => {
                assert_eq!(from, PermissionMode::Default);
                assert_eq!(to, PermissionMode::AcceptEdits);
            }
            _ => panic!("expected PermissionModeChanged"),
        }
    }

    #[test]
    fn from_event_task_updated() {
        let task = Task::new("do something".to_string());
        let dto = EventDto::from_event(10, &Event::TaskUpdated { task: task.clone() });
        match dto.kind {
            EventKind::TaskUpdated { task: t } => {
                assert_eq!(t.content, task.content);
            }
            _ => panic!("expected TaskUpdated"),
        }
    }

    #[test]
    fn from_event_config_changed() {
        let dto = EventDto::from_event(11, &Event::ConfigChanged);
        assert!(matches!(dto.kind, EventKind::ConfigChanged));
    }

    // ---- EventKind::from_persisted 全变体覆盖 ----

    #[test]
    fn from_persisted_session_created() {
        let p = PersistedEvent::SessionCreated {
            id: "s1".to_string(),
            workdir: "/tmp".to_string(),
            config_hash: 42,
            created_at: OffsetDateTime::now_utc(),
        };
        let kind = EventKind::from_persisted(&p);
        match kind {
            EventKind::SessionCreated { id } => assert_eq!(id, "s1"),
            _ => panic!("expected SessionCreated"),
        }
    }

    #[test]
    fn from_persisted_message_appended() {
        let p = PersistedEvent::MessageAppended {
            message: Message::user_text("hello"),
        };
        let kind = EventKind::from_persisted(&p);
        assert!(matches!(kind, EventKind::MessageAppended { .. }));
    }

    #[test]
    fn from_persisted_permission_resolved() {
        let p = PersistedEvent::PermissionResolved {
            id: "p1".to_string(),
            decision: Decision::Allow,
        };
        let kind = EventKind::from_persisted(&p);
        match kind {
            EventKind::PermissionResolved { id, decision } => {
                assert_eq!(id, "p1");
                assert!(matches!(decision, Decision::Allow));
            }
            _ => panic!("expected PermissionResolved"),
        }
    }

    #[test]
    fn from_persisted_permission_mode_changed() {
        let p = PersistedEvent::PermissionModeChanged {
            from: PermissionMode::Plan,
            to: PermissionMode::Default,
        };
        let kind = EventKind::from_persisted(&p);
        match kind {
            EventKind::PermissionModeChanged { from, to } => {
                assert_eq!(from, PermissionMode::Plan);
                assert_eq!(to, PermissionMode::Default);
            }
            _ => panic!("expected PermissionModeChanged"),
        }
    }

    #[test]
    fn from_persisted_task_updated() {
        let p = PersistedEvent::TaskUpdated {
            task: Task::new("task".to_string()),
        };
        let kind = EventKind::from_persisted(&p);
        assert!(matches!(kind, EventKind::TaskUpdated { .. }));
    }

    #[test]
    fn from_persisted_turn_end() {
        let p = PersistedEvent::TurnEnd {
            stop_reason: StopReason::Stopped,
        };
        let kind = EventKind::from_persisted(&p);
        match kind {
            EventKind::TurnEnd { stop_reason } => {
                assert_eq!(stop_reason, StopReason::Stopped);
            }
            _ => panic!("expected TurnEnd"),
        }
    }

    // ---- 序列化 roundtrip（各变体）----

    #[test]
    fn turn_streaming_started_roundtrip() {
        let dto = EventDto {
            seq: 1,
            kind: EventKind::TurnStreamingStarted,
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"type\":\"turn_streaming_started\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.kind, EventKind::TurnStreamingStarted));
    }

    #[test]
    fn turn_end_roundtrip() {
        let dto = EventDto {
            seq: 2,
            kind: EventKind::TurnEnd {
                stop_reason: StopReason::Interrupted,
            },
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"type\":\"turn_end\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        match back.kind {
            EventKind::TurnEnd { stop_reason } => {
                assert_eq!(stop_reason, StopReason::Interrupted);
            }
            _ => panic!("expected TurnEnd"),
        }
    }

    #[test]
    fn session_created_roundtrip() {
        let dto = EventDto {
            seq: 3,
            kind: EventKind::SessionCreated {
                id: "sess-xyz".to_string(),
            },
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"type\":\"session_created\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        match back.kind {
            EventKind::SessionCreated { id } => assert_eq!(id, "sess-xyz"),
            _ => panic!("expected SessionCreated"),
        }
    }

    #[test]
    fn permission_requested_roundtrip() {
        let dto = EventDto {
            seq: 4,
            kind: EventKind::PermissionRequested {
                id: "p1".to_string(),
                tool: "shell.run".to_string(),
                summary: "rm -rf".to_string(),
                risk: Risk::High,
            },
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"type\":\"permission_requested\""));
        assert!(json.contains("\"risk\":\"high\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        match back.kind {
            EventKind::PermissionRequested { risk, .. } => assert_eq!(risk, Risk::High),
            _ => panic!("expected PermissionRequested"),
        }
    }

    #[test]
    fn permission_resolved_roundtrip() {
        let dto = EventDto {
            seq: 5,
            kind: EventKind::PermissionResolved {
                id: "p1".to_string(),
                decision: Decision::Allow,
            },
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"type\":\"permission_resolved\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        match back.kind {
            EventKind::PermissionResolved { decision, .. } => {
                assert!(matches!(decision, Decision::Allow));
            }
            _ => panic!("expected PermissionResolved"),
        }
    }

    #[test]
    fn permission_mode_changed_roundtrip() {
        let dto = EventDto {
            seq: 6,
            kind: EventKind::PermissionModeChanged {
                from: PermissionMode::Default,
                to: PermissionMode::Plan,
            },
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"type\":\"permission_mode_changed\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        match back.kind {
            EventKind::PermissionModeChanged { from, to } => {
                assert_eq!(from, PermissionMode::Default);
                assert_eq!(to, PermissionMode::Plan);
            }
            _ => panic!("expected PermissionModeChanged"),
        }
    }

    #[test]
    fn config_changed_roundtrip() {
        let dto = EventDto {
            seq: 7,
            kind: EventKind::ConfigChanged,
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"type\":\"config_changed\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.kind, EventKind::ConfigChanged));
    }

    #[test]
    fn sessions_listed_roundtrip() {
        let dto = EventDto {
            seq: 8,
            kind: EventKind::SessionsListed {
                sessions: Vec::new(),
            },
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"type\":\"sessions_listed\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.kind, EventKind::SessionsListed { .. }));
    }

    #[test]
    fn session_retrieved_roundtrip() {
        let dto = EventDto {
            seq: 9,
            kind: EventKind::SessionRetrieved {
                session_id: "s1".to_string(),
                messages: Vec::new(),
            },
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"type\":\"session_retrieved\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        match back.kind {
            EventKind::SessionRetrieved { session_id, .. } => {
                assert_eq!(session_id, "s1");
            }
            _ => panic!("expected SessionRetrieved"),
        }
    }

    #[test]
    fn command_error_roundtrip() {
        let dto = EventDto {
            seq: 0,
            kind: EventKind::CommandError {
                message: "bad request".to_string(),
            },
        };
        let json = serde_json::to_string(&dto).unwrap();
        assert!(json.contains("\"type\":\"command_error\""));
        let back: EventDto = serde_json::from_str(&json).unwrap();
        match back.kind {
            EventKind::CommandError { message } => assert_eq!(message, "bad request"),
            _ => panic!("expected CommandError"),
        }
    }

    #[test]
    fn from_event_seq_is_preserved() {
        // 验证 from_event 将 seq 正确填入 DTO。
        let event = Event::ConfigChanged;
        let dto = EventDto::from_event(999, &event);
        assert_eq!(dto.seq, 999);
    }
}
