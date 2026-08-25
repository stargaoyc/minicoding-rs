//! 事件重放：从 snapshot + 事件流重建 `Session` 状态。
//!
//! ## 重放流程
//!
//! 1. 加载最近 `SessionSnapshot`（如有），获得初始 `SessionState`；
//! 2. 加载 `seq > snapshot.seq` 的事件流（无 snapshot 时加载全部事件）；
//! 3. 按事件顺序应用：
//!    - `SessionCreated`：初始化 `Session`（仅当无 snapshot 时）；
//!    - `MessageAppended`：追加到 `messages`；
//!    - `PermissionResolved`/`PermissionModeChanged`/`TaskUpdated`/`TurnEnd`：
//!      仅记录审计轨迹，不重建运行时状态（决策已生效，重放无副作用）；
//! 4. 返回重建后的 `Session` + 审计轨迹。
//!
//! ## 旧会话兼容
//!
//! 若 `EventStore::load` 返回空（旧会话无事件流），调用方应回退到
//! `Storage::load` 加载消息列表，构造空 `Session` 后填入 `messages`。
//!
//! 详见 `design.md` §25.4。

use minicoding_core::model::{Message, RuntimeError, Session, SessionId};
use minicoding_core::policy::PermissionMode;
use minicoding_core::storage::SessionSnapshot;
use minicoding_core::storage::{EventRecord, PersistedEvent};

/// 重放错误。
#[derive(thiserror::Error, Debug)]
pub enum ReplayError {
    /// 事件流缺少 `SessionCreated`（首事件非 `SessionCreated` 且无 snapshot）。
    #[error("missing SessionCreated event (no snapshot and first event is not SessionCreated)")]
    MissingSessionCreated,
    /// schema 版本不支持（未来版本）。
    #[error("unsupported schema version: {0} (current: {1})")]
    UnsupportedSchema(u32, u32),
    /// 事件 seq 不连续（seq 跳跃，可能事件丢失）。
    #[error("event seq gap: expected {expected}, got {actual}")]
    SeqGap { expected: u64, actual: u64 },
}

impl From<ReplayError> for RuntimeError {
    fn from(e: ReplayError) -> Self {
        RuntimeError::Storage(minicoding_core::model::StorageError::Corrupted(
            e.to_string(),
        ))
    }
}

/// 重放结果：重建后的 `Session` + 审计轨迹。
#[derive(Debug, Clone)]
pub struct ReplayedSession {
    /// 重建后的会话状态。
    pub session: Session,
    /// 重放过程中遇到的权限决策/模式切换/任务更新/turn 结束事件（审计回放用）。
    pub audit_trail: Vec<PersistedEvent>,
    /// 重放的最大 seq（= 最后一条事件的 seq，或 snapshot.seq 若无后续事件）。
    pub last_seq: u64,
    /// 重建最终态的权限模式（默认 `Default`，被 `PermissionModeChanged` 覆盖）。
    pub final_permission_mode: PermissionMode,
}

/// 从 snapshot + 事件流重建 `Session` 状态。
///
/// ## 参数
///
/// - `snapshot`：最近 snapshot（`None` 表示无 snapshot，从空状态重放）；
/// - `events`：事件流（按 seq 升序）。`snapshot` 为 `Some` 时仅需 `seq >
///   snapshot.seq` 的事件；`None` 时需全部事件（首事件必须为 `SessionCreated`）。
///
/// # Errors
///
/// - `MissingSessionCreated`：无 snapshot 且首事件非 `SessionCreated`；
/// - `UnsupportedSchema`：事件或 snapshot 的 schema 版本高于当前；
/// - `SeqGap`：事件 seq 跳跃（可能事件丢失）。
///
/// ## 旧会话兼容
///
/// 若 `events` 为空且 `snapshot` 为 `None`，返回 `MissingSessionCreated`——
/// 调用方应捕获此错误并回退到 `Storage::load` 消息列表路径。
pub fn replay_session_state(
    snapshot: Option<&SessionSnapshot>,
    events: Vec<EventRecord>,
) -> Result<ReplayedSession, ReplayError> {
    const CURRENT_SCHEMA: u32 = minicoding_core::storage::SCHEMA_VERSION;

    let mut session: Option<Session> = snapshot.as_ref().map(|s| Session {
        id: s.state.id.clone(),
        created_at: s.state.created_at,
        workdir: camino::Utf8PathBuf::from(&s.state.workdir),
        config_hash: s.state.config_hash,
        messages: s.state.messages.clone(),
    });
    let mut last_seq = snapshot.as_ref().map_or(0, |s| s.seq);
    let mut audit_trail: Vec<PersistedEvent> = Vec::new();
    let mut final_permission_mode = PermissionMode::Default;

    // snapshot schema 版本检查
    if let Some(snap) = snapshot
        && snap.schema_version > CURRENT_SCHEMA
    {
        return Err(ReplayError::UnsupportedSchema(
            snap.schema_version,
            CURRENT_SCHEMA,
        ));
    }

    for record in events {
        // seq 连续性检查（snapshot 之后首事件应为 `last_seq + 1`）
        let expected = last_seq + 1;
        if record.seq != expected {
            return Err(ReplayError::SeqGap {
                expected,
                actual: record.seq,
            });
        }
        // schema 版本检查
        if record.schema_version > CURRENT_SCHEMA {
            return Err(ReplayError::UnsupportedSchema(
                record.schema_version,
                CURRENT_SCHEMA,
            ));
        }

        match &record.event {
            PersistedEvent::SessionCreated {
                id,
                workdir,
                config_hash,
                created_at,
            } => {
                if session.is_none() {
                    session = Some(Session {
                        id: id.clone(),
                        created_at: *created_at,
                        workdir: camino::Utf8PathBuf::from(workdir),
                        config_hash: *config_hash,
                        messages: Vec::new(),
                    });
                }
                // 有 snapshot 时 `SessionCreated` 已被 snapshot 包含，跳过
            }
            PersistedEvent::MessageAppended { message } => {
                let Some(s) = session.as_mut() else {
                    return Err(ReplayError::MissingSessionCreated);
                };
                s.messages.push(message.clone());
            }
            PersistedEvent::PermissionResolved { .. }
            | PersistedEvent::TaskUpdated { .. }
            | PersistedEvent::TurnEnd { .. } => {
                // 仅记录审计轨迹，不重建运行时状态
                audit_trail.push(record.event.clone());
            }
            PersistedEvent::PermissionModeChanged { to, .. } => {
                final_permission_mode = *to;
                audit_trail.push(record.event.clone());
            }
            // M-06（SCHEMA_VERSION 2）：step 边界事件仅 log 定位（压缩点/中断点），
            // 不重建任何状态。v1 事件流无此变体；此处显式匹配保持 forward-compat。
            PersistedEvent::StepStarted { .. } | PersistedEvent::StepEnded { .. } => {}
        }
        last_seq = record.seq;
    }

    let session = session.ok_or(ReplayError::MissingSessionCreated)?;
    // 防御修复（M-03，D-05）：重放历史中仍悬空的 tool_calls 补合成错误结果，
    // 保证重建出的会话对严格 provider 合法（幂等：已齐不动）。
    let mut session = session;
    session.messages =
        minicoding_core::runtime::repair::repair_dangling_tool_calls(session.messages);
    Ok(ReplayedSession {
        session,
        audit_trail,
        last_seq,
        final_permission_mode,
    })
}

/// 从消息列表构造 `Session`（旧会话兼容路径，无事件流时使用）。
///
/// 当 `EventStore::load` 返回空（旧 `{id}.jsonl` 会话无事件流）时，调用方
/// 用此函数从 `Storage::load` 的消息列表构造 `Session`，避免老会话不可用。
///
/// `workdir`/`config_hash` 由调用方提供（CLI 启动时已知）。
#[must_use]
pub fn session_from_messages(
    id: SessionId,
    workdir: camino::Utf8PathBuf,
    config_hash: u64,
    messages: Vec<Message>,
) -> Session {
    // 防御修复（M-03，D-05）：旧会话回退路径同样补齐悬空 tool_calls 的合成结果。
    let messages = minicoding_core::runtime::repair::repair_dangling_tool_calls(messages);
    let created_at = messages
        .first()
        .map_or_else(time::OffsetDateTime::now_utc, |m| m.created_at);
    Session {
        id,
        created_at,
        workdir,
        config_hash,
        messages,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::model::Message;
    use minicoding_core::storage::{EventRecord, PersistedEvent, SCHEMA_VERSION};
    use minicoding_core::storage::{SessionSnapshot, SessionState};
    use time::OffsetDateTime;

    fn make_session_created(seq: u64, id: &str) -> EventRecord {
        EventRecord::new(
            seq,
            id.to_string(),
            PersistedEvent::SessionCreated {
                id: id.to_string(),
                workdir: "/tmp/proj".to_string(),
                config_hash: 12345,
                created_at: OffsetDateTime::UNIX_EPOCH,
            },
        )
    }

    fn make_message_appended(seq: u64, id: &str, text: &str) -> EventRecord {
        EventRecord::new(
            seq,
            id.to_string(),
            PersistedEvent::MessageAppended {
                message: Message::user_text(text),
            },
        )
    }

    #[test]
    fn replay_from_empty_state_with_session_created() {
        let id = "01TEST";
        let events = vec![
            make_session_created(1, id),
            make_message_appended(2, id, "hello"),
            make_message_appended(3, id, "world"),
        ];
        let result = replay_session_state(None, events).unwrap();
        assert_eq!(result.session.id, id);
        assert_eq!(result.session.messages.len(), 2);
        assert_eq!(result.session.messages[0].text(), "hello");
        assert_eq!(result.session.messages[1].text(), "world");
        assert_eq!(result.last_seq, 3);
        assert_eq!(result.final_permission_mode, PermissionMode::Default);
    }

    #[test]
    fn replay_missing_session_created_errors() {
        let id = "01TEST";
        // 首事件非 SessionCreated → 错误
        let events = vec![make_message_appended(1, id, "hello")];
        let result = replay_session_state(None, events);
        assert!(matches!(result, Err(ReplayError::MissingSessionCreated)));
    }

    #[test]
    fn replay_with_snapshot_skips_session_created() {
        let id = "01SNAP";
        let state = SessionState {
            id: id.to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            workdir: "/tmp/proj".to_string(),
            config_hash: 12345,
            messages: vec![Message::user_text("snapshot-msg")],
            permission_mode: None,
            sandbox_preset: None,
        };
        let snapshot = SessionSnapshot::new(1, state);
        // snapshot.seq=1，后续事件从 seq=2 开始
        let events = vec![
            make_message_appended(2, id, "post-snap-1"),
            make_message_appended(3, id, "post-snap-2"),
        ];
        let result = replay_session_state(Some(&snapshot), events).unwrap();
        assert_eq!(result.session.messages.len(), 3);
        assert_eq!(result.session.messages[0].text(), "snapshot-msg");
        assert_eq!(result.session.messages[1].text(), "post-snap-1");
        assert_eq!(result.session.messages[2].text(), "post-snap-2");
        assert_eq!(result.last_seq, 3);
    }

    #[test]
    fn replay_seq_gap_errors() {
        let id = "01GAP";
        let events = vec![
            make_session_created(1, id),
            make_message_appended(3, id, "skipped seq 2"), // seq 跳跃
        ];
        let result = replay_session_state(None, events);
        assert!(matches!(
            result,
            Err(ReplayError::SeqGap {
                expected: 2,
                actual: 3
            })
        ));
    }

    #[test]
    fn replay_unsupported_schema_errors() {
        let id = "01FUTURE";
        let mut record = make_session_created(1, id);
        record.schema_version = SCHEMA_VERSION + 1; // 未来版本
        let result = replay_session_state(None, vec![record]);
        assert!(matches!(
            result,
            Err(ReplayError::UnsupportedSchema(v, _)) if v == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn replay_permission_mode_changed_updates_final_mode() {
        let id = "01MODE";
        let events = vec![
            make_session_created(1, id),
            EventRecord::new(
                2,
                id.to_string(),
                PersistedEvent::PermissionModeChanged {
                    from: PermissionMode::Default,
                    to: PermissionMode::Plan,
                },
            ),
        ];
        let result = replay_session_state(None, events).unwrap();
        assert_eq!(result.final_permission_mode, PermissionMode::Plan);
        assert_eq!(result.audit_trail.len(), 1);
    }

    #[test]
    fn replay_empty_events_without_snapshot_errors() {
        let result = replay_session_state(None, Vec::new());
        assert!(matches!(result, Err(ReplayError::MissingSessionCreated)));
    }

    #[test]
    fn replay_handles_v2_step_events_ignored() {
        // M-06：step 边界事件不重建状态、不进 audit_trail，仅消费 seq
        let id = "01STEP";
        let events = vec![
            make_session_created(1, id),
            EventRecord::new(
                2,
                id.to_string(),
                PersistedEvent::StepStarted {
                    iter: 0,
                    tool_call_ids: vec!["call_a".to_string()],
                },
            ),
            make_message_appended(3, id, "assistant with tool call"),
            EventRecord::new(4, id.to_string(), PersistedEvent::StepEnded { iter: 0 }),
        ];
        let result = replay_session_state(None, events).unwrap();
        assert_eq!(result.session.messages.len(), 1);
        assert!(result.audit_trail.is_empty(), "step 事件不进审计轨迹");
        assert_eq!(result.last_seq, 4);
    }

    #[test]
    fn replay_handles_v1_without_step_events() {
        // v1 兼容：无 step 事件的事件流（旧会话）replay 正常
        let id = "01V1";
        let events = vec![
            make_session_created(1, id),
            make_message_appended(2, id, "v1 message"),
        ];
        let result = replay_session_state(None, events).unwrap();
        assert_eq!(result.session.messages.len(), 1);
        assert_eq!(result.last_seq, 2);
        // 旧会话按 v1 语义重建（Step 事件缺席即无 step 边界，行为与 v1 一致）
    }

    #[test]
    fn replay_empty_events_with_snapshot_returns_snapshot_state() {
        let id = "01SNAPONLY";
        let state = SessionState {
            id: id.to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            workdir: "/tmp".to_string(),
            config_hash: 0,
            messages: vec![Message::user_text("snap")],
            permission_mode: None,
            sandbox_preset: None,
        };
        let snapshot = SessionSnapshot::new(5, state);
        let result = replay_session_state(Some(&snapshot), Vec::new()).unwrap();
        assert_eq!(result.session.messages.len(), 1);
        assert_eq!(result.last_seq, 5);
    }

    #[test]
    fn session_from_messages_construction() {
        let msgs = vec![Message::user_text("a"), Message::assistant_text("b")];
        let session = session_from_messages(
            "01FROMMSG".to_string(),
            camino::Utf8PathBuf::from("/tmp"),
            999,
            msgs,
        );
        assert_eq!(session.id, "01FROMMSG");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.config_hash, 999);
        // created_at 取首条消息时间
        assert_ne!(session.created_at, OffsetDateTime::UNIX_EPOCH);
    }
}
