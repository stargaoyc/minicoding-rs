//! Event Sourcing：事件持久化与重放（见 `design.md` §25）。
//!
//! 将会话状态建模为不可变事件流：每个状态变更（消息追加、权限决策、模式切换等）
//! 记录为 `EventRecord`，按 seq 单调递增持久化。`replay_session_state` 从 snapshot +
//! 事件流重建 `Session` 状态。
//!
//! ## 设计要点
//!
//! - **schema 版本化**：`EventRecord.schema_version` 标记事件结构版本，旧版会话
//!   通过 migration 适配（当前 v1，见 `SCHEMA_VERSION`）；
//! - **持久化事件子集**：仅持久化状态变更事件（`SessionCreated`/`MessageAppended`/
//!   `PermissionResolved`/`PermissionModeChanged`/`TaskUpdated`/`TurnEnd`），
//!   跳过瞬态事件（`Token`/`TurnStreamingStarted`/`ToolCallStarted`/
//!   `ToolCallFinished`/`PermissionRequested`/`ConfigChanged`）——后者或为流式增量
//!   （已被 `MessageAppended` 捕获）、或为通知类（无状态变更）；
//! - **JSONL 后端**：`minicoding-storage::JsonlEventStore` 每会话一文件
//!   `{id}.events.jsonl`，追加写 + fsync；
//! - **崩溃安全**：事件先落盘再广播（与现有 `storage.append` 一致），崩溃时磁盘
//!   状态为已持久化事件的子集；
//! - **SSE 协同**：`EventCursor.durable_seq` 升级为真实持久化进度，ring buffer
//!   evict 时回退 `EventStore::load_after` 重放（替代 `RehydrateRequired`，
//!   见 `protocol::cursor`）；
//! - **旧会话兼容**：旧 `{id}.jsonl`（消息日志）仍可用；新会话双写（messages +
//!   events）平滑过渡。`replay_session_state` 在无事件流时回退到消息列表。
//!
//! 详见 `design.md` §25、`data-model.md` §5。

use crate::model::{Message, SessionId, StopReason, Task};
use crate::policy::{Decision, PermissionMode};
use crate::provider::BoxFuture;
use crate::storage::StorageError;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 当前事件 schema 版本。
///
/// 旧版会话通过 migration 适配：
/// - v0（隐式）：消息日志（无 `EventRecord` 包装），`replay_session_state` 回退
///   到消息列表；
/// - v1：`EventRecord` 显式包装 `PersistedEvent`（无 step 边界事件）；
/// - v2：新增 `StepStarted`/`StepEnded`（M-06，step 边界定位，仅 log 不进
///   transcript）。`replay_session_state` 对 v1 事件流跳过 Step 处理（Step 事件
///   不影响消息重建，向后兼容）；
/// - v3：`SessionState` 新增会话安全上下文字段 `permission_mode`/`sandbox_preset`
///   （FE-7，快照持久化权限模式与沙箱 preset）。字段均 `#[serde(default)]`，
///   旧快照缺字段兼容读 `None`；事件变体不变，v1/v2 事件流照常重放。
///
/// 未来变更（如新增事件变体、字段语义变更）递增此版本号，并在
/// `replay_session_state` 中按版本分支处理。
pub const SCHEMA_VERSION: u32 = 3;

/// 持久化事件种类（`Event` 子集，仅状态变更类 + step 定位类）。
///
/// 与 `runtime::Event` 的区别：
/// - 仅包含状态变更事件（replay 后可重建 `Session`）与 step 边界事件
///   （`StepStarted`/`StepEnded`，M-06：仅 log 定位，不重建状态）；
/// - 不含瞬态事件（`Token`/`TurnStreamingStarted`/`ToolCallStarted`/
///   `ToolCallFinished`/`PermissionRequested`/`ConfigChanged`）；
/// - `SessionCreated` 携带 `workdir`/`config_hash`/`created_at`（重建 `Session` 必需）。
///
/// 序列化为 tagged enum（`tag = "type"`），与 `protocol::EventKind` 风格一致，
/// 便于跨进程传输与 schema 演进。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PersistedEvent {
    /// 会话创建（初始化 `workdir`/`config_hash`/`created_at`）。
    ///
    /// 与 `runtime::Event::SessionCreated` 的区别：携带 `workdir`/`config_hash`/`created_at`
    /// 以便从空事件流重建 `Session`。原 `Event::SessionCreated` 仅含 id（运行时
    /// `Session` 已存在，无需重建）。
    SessionCreated {
        id: SessionId,
        workdir: String,
        config_hash: u64,
        #[serde(with = "time::serde::rfc3339")]
        created_at: OffsetDateTime,
    },
    /// 一条消息已追加（落盘 + 入上下文后）。
    ///
    /// 与 `runtime::Event::MessageAppended` 一一映射。replay 时按顺序拼回
    /// `Session.messages`。
    MessageAppended { message: Message },
    /// 权限已 resolved（带最终决策，供审计回放）。
    ///
    /// 与 `runtime::Event::PermissionResolved` 一一映射。replay 时仅记录审计
    /// 轨迹，不重建运行时状态（决策已生效，不需重放）。
    PermissionResolved { id: String, decision: Decision },
    /// 权限模式切换（`plan.exit` / `/plan` / `--plan` 触发）。
    ///
    /// 与 `runtime::Event::PermissionModeChanged` 一一映射。replay 时重建
    /// `PlanModeSnapshot.mode`（不重建 `allowed_prompts`——预批准缓存仅在
    /// 当前 turn 有效，跨会话不保留）。
    PermissionModeChanged {
        from: PermissionMode,
        to: PermissionMode,
    },
    /// 任务更新（`task.create`/`task.update` 后广播）。
    ///
    /// 与 `runtime::Event::TaskUpdated` 一一映射。replay 时重建任务列表
    /// （按 task id 去重，后者覆盖前者）。
    TaskUpdated { task: Task },
    /// 一轮结束（含停止原因）。
    ///
    /// 与 `runtime::Event::TurnEnd` 一一映射。replay 时仅记录审计轨迹。
    TurnEnd { stop_reason: StopReason },
    /// step 开始（M-06，SCHEMA_VERSION 2+）：一次 LLM 请求 + 其触发的工具调用。
    ///
    /// 仅 log 定位（压缩点/中断点），replay 时不重建任何状态（C-05：不进
    /// transcript，模型不可见）。v1 事件流无此变体，replay 跳过。
    StepStarted {
        iter: u32,
        tool_call_ids: Vec<String>,
    },
    /// step 结束（M-06，SCHEMA_VERSION 2+）：该次迭代工具结果已全部回灌。
    ///
    /// cancel/timeout 中断时可能只有 `StepStarted` 无 `StepEnded`（中断点定位）。
    StepEnded { iter: u32 },
}

/// 持久化事件记录（携带 seq + schema 元数据）。
///
/// 每会话 seq 单调递增（从 1 开始），SSE cursor 用 seq 做 cursor 恢复。
/// `schema_version` 标记事件结构版本，旧版通过 migration 适配（见
/// `SCHEMA_VERSION`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    /// 单调递增序列号（每会话独立计数，从 1 开始）。
    pub seq: u64,
    /// 会话 ID。
    pub session_id: SessionId,
    /// 事件发生时间（UTC）。
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// 事件 schema 版本（见 `SCHEMA_VERSION`）。
    pub schema_version: u32,
    /// 事件种类。
    #[serde(flatten)]
    pub event: PersistedEvent,
}

impl EventRecord {
    /// 构造新事件记录（自动填入当前 schema 版本与时间戳）。
    #[must_use]
    pub fn new(seq: u64, session_id: SessionId, event: PersistedEvent) -> Self {
        Self {
            seq,
            session_id,
            timestamp: OffsetDateTime::now_utc(),
            schema_version: SCHEMA_VERSION,
            event,
        }
    }
}

/// 事件存储 trait（`dyn` 兼容）。
///
/// 实现见 `minicoding_storage::JsonlEventStore`（JSONL 后端）。
///
/// ## 崩溃安全
///
/// `append` 必须在返回前 `fsync`，保证崩溃时磁盘状态为已持久化事件的子集。
/// `load`/`load_after` 按 seq 升序返回。
///
/// ## seq 分配
///
/// `next_seq` 返回下一个可分配的 seq（= 当前最大 seq + 1）。调用方负责保证
/// seq 单调递增（Runtime 持有 `EventCursor`，由其分配 seq 后调 `append`）。
pub trait EventStore: Send + Sync {
    /// 追加事件记录（fsync 后返回）。
    ///
    /// # Errors
    /// - `StorageError::Io`：写入或 fsync 失败；
    /// - `StorageError::Serialize`：序列化失败。
    fn append(
        &self,
        session: &SessionId,
        record: EventRecord,
    ) -> BoxFuture<'_, Result<(), StorageError>>;

    /// 加载会话全部事件（按 seq 升序）。
    ///
    /// # Errors
    /// - `StorageError::Io`：读取失败（除 `NotFound`）；
    /// - `StorageError::Corrupted`：事件行 JSON 解析失败。
    fn load(&self, session: &SessionId) -> BoxFuture<'_, Result<Vec<EventRecord>, StorageError>>;

    /// 加载 `after_seq` 之后的事件（不含 `after_seq`，按 seq 升序）。
    ///
    /// 用于 SSE cursor 恢复：客户端带 `Last-Event-ID: <seq>` 请求时，server
    /// 调 `load_after(seq)` 重放后续事件。
    ///
    /// # Errors
    /// 同 `load`。
    fn load_after(
        &self,
        session: &SessionId,
        after_seq: u64,
    ) -> BoxFuture<'_, Result<Vec<EventRecord>, StorageError>>;

    /// 返回下一个可分配的 seq（= 当前最大 seq + 1，空会话返回 1）。
    ///
    /// # Errors
    /// 读取失败时返回 `StorageError`。
    fn next_seq(&self, session: &SessionId) -> BoxFuture<'_, Result<u64, StorageError>>;

    /// 删除会话全部事件。
    ///
    /// # Errors
    /// 文件删除失败（除 `NotFound`）时返回错误。
    fn delete(&self, session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>>;
}

/// 无操作事件存储（兜底，未启用 event sourcing 时使用）。
///
/// `append` 为空操作，`load` 返回空 Vec，`next_seq` 返回 1。仅用于测试或未启用
/// event sourcing 的场景。
pub struct NoopEventStore;

impl EventStore for NoopEventStore {
    fn append(
        &self,
        _session: &SessionId,
        _record: EventRecord,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        Box::pin(async move { Ok(()) })
    }

    fn load(&self, _session: &SessionId) -> BoxFuture<'_, Result<Vec<EventRecord>, StorageError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }

    fn load_after(
        &self,
        _session: &SessionId,
        _after_seq: u64,
    ) -> BoxFuture<'_, Result<Vec<EventRecord>, StorageError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }

    fn next_seq(&self, _session: &SessionId) -> BoxFuture<'_, Result<u64, StorageError>> {
        Box::pin(async move { Ok(1) })
    }

    fn delete(&self, _session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        Box::pin(async move { Ok(()) })
    }
}

/// 判断 `runtime::Event` 是否为持久化事件（`PersistedEvent` 子集）。
///
/// Runtime 持有 `EventBus` 广播全量 `Event`，但只有持久化事件子集需写入
/// `EventStore`。此函数用于过滤。
///
/// 返回 `Some(PersistedEvent)` 表示应持久化，`None` 表示瞬态事件（跳过）。
///
/// # 注意
///
/// `runtime::Event::SessionCreated` 仅含 `id`，不携带 `workdir`/`config_hash`/
/// `created_at`；调用方应改用 `PersistedEvent::SessionCreated` 的完整字段
/// （从 `Session` 结构构造）。此函数对 `SessionCreated` 返回 `None`，
/// 由 Runtime 在会话创建时显式构造完整 `PersistedEvent`。
#[must_use]
pub fn try_persist(event: &crate::runtime::Event) -> Option<PersistedEvent> {
    match event {
        crate::runtime::Event::MessageAppended(msg) => Some(PersistedEvent::MessageAppended {
            message: msg.clone(),
        }),
        crate::runtime::Event::PermissionResolved { id, decision } => {
            Some(PersistedEvent::PermissionResolved {
                id: id.clone(),
                decision: decision.clone(),
            })
        }
        crate::runtime::Event::PermissionModeChanged { from, to } => {
            Some(PersistedEvent::PermissionModeChanged {
                from: *from,
                to: *to,
            })
        }
        crate::runtime::Event::TaskUpdated { task } => {
            Some(PersistedEvent::TaskUpdated { task: task.clone() })
        }
        crate::runtime::Event::TurnEnd { stop_reason } => Some(PersistedEvent::TurnEnd {
            stop_reason: stop_reason.clone(),
        }),
        crate::runtime::Event::StepStarted {
            iter,
            tool_call_ids,
        } => Some(PersistedEvent::StepStarted {
            iter: *iter,
            tool_call_ids: tool_call_ids.clone(),
        }),
        crate::runtime::Event::StepEnded { iter } => {
            Some(PersistedEvent::StepEnded { iter: *iter })
        }
        // 瞬态事件：Token / ReasoningDelta / TurnStreamingStarted / ToolCallStarted /
        // ToolCallFinished / PermissionRequested / ConfigChanged / SessionCreated
        // SessionCreated 由 Runtime 显式构造完整 PersistedEvent（携带 workdir 等）
        crate::runtime::Event::Token(_)
        | crate::runtime::Event::ReasoningDelta(_)
        | crate::runtime::Event::TurnStreamingStarted
        | crate::runtime::Event::ToolCallStarted { .. }
        | crate::runtime::Event::ToolCallFinished { .. }
        | crate::runtime::Event::PermissionRequested { .. }
        | crate::runtime::Event::SessionCreated { .. }
        | crate::runtime::Event::ConfigChanged => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use crate::model::Message;
    use crate::runtime::Event;

    #[test]
    fn try_persist_filters_transient_events() {
        // 瞬态事件被过滤
        assert!(try_persist(&Event::Token("hi".into())).is_none());
        assert!(try_persist(&Event::TurnStreamingStarted).is_none());
        assert!(try_persist(&Event::ConfigChanged).is_none());
        assert!(
            try_persist(&Event::SessionCreated { id: "x".into() }).is_none(),
            "SessionCreated 由 Runtime 显式构造完整 PersistedEvent"
        );

        // 持久化事件保留
        let msg = Message::user_text("hello");
        assert!(matches!(
            try_persist(&Event::MessageAppended(msg)),
            Some(PersistedEvent::MessageAppended { .. })
        ));
    }

    #[test]
    fn event_record_roundtrip() {
        let msg = Message::user_text("test");
        let record = EventRecord::new(
            42,
            "01TEST".to_string(),
            PersistedEvent::MessageAppended { message: msg },
        );
        let json = serde_json::to_string(&record).unwrap();
        let back: EventRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 42);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert!(matches!(back.event, PersistedEvent::MessageAppended { .. }));
    }

    #[test]
    fn schema_version_bumped_to_3() {
        // M-06：v2 引入 StepStarted/StepEnded 变体；
        // FE-7：v3 引入 SessionState 安全上下文字段（permission_mode/sandbox_preset）
        assert_eq!(SCHEMA_VERSION, 3);
    }

    #[test]
    fn step_events_roundtrip_with_tagged_serde() {
        // step 事件序列化按 tag="type" snake_case，协议层可识别
        let started = PersistedEvent::StepStarted {
            iter: 1,
            tool_call_ids: vec!["call_a".to_string()],
        };
        let json = serde_json::to_string(&started).unwrap();
        assert!(json.contains("\"type\":\"step_started\""), "{json}");
        let back: PersistedEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            PersistedEvent::StepStarted {
                iter: 1,
                tool_call_ids,
            } if tool_call_ids == ["call_a"]
        ));

        let ended = PersistedEvent::StepEnded { iter: 1 };
        let json = serde_json::to_string(&ended).unwrap();
        assert!(json.contains("\"type\":\"step_ended\""), "{json}");
        let back: PersistedEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, PersistedEvent::StepEnded { iter: 1 }));
    }

    #[test]
    fn try_persist_maps_step_events() {
        use crate::runtime::Event;
        let started = Event::StepStarted {
            iter: 2,
            tool_call_ids: vec!["call_x".to_string(), "call_y".to_string()],
        };
        assert!(matches!(
            try_persist(&started),
            Some(PersistedEvent::StepStarted {
                iter: 2,
                tool_call_ids,
            }) if tool_call_ids == ["call_x", "call_y"]
        ));
        assert!(matches!(
            try_persist(&Event::StepEnded { iter: 2 }),
            Some(PersistedEvent::StepEnded { iter: 2 })
        ));
    }

    #[tokio::test]
    async fn noop_event_store_returns_empty() {
        let store = NoopEventStore;
        let id = "01NOOP".to_string();
        let records = store.load(&id).await.unwrap();
        assert!(records.is_empty(), "expected empty: records");
        let next = store.next_seq(&id).await.unwrap();
        assert_eq!(next, 1);
    }

    #[tokio::test]
    async fn noop_event_store_append_is_noop() {
        let store = NoopEventStore;
        let id = "01NOOP".to_string();
        let record = EventRecord::new(
            1,
            id.clone(),
            PersistedEvent::MessageAppended {
                message: Message::user_text("x"),
            },
        );
        store.append(&id, record).await.unwrap();
        // append 后 load 仍为空（no-op）
        let records = store.load(&id).await.unwrap();
        assert!(records.is_empty(), "expected empty: records");
    }
}
