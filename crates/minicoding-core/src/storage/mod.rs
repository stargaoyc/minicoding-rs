//! Storage 模块 re-export。
//!
//! M0-M8：`Storage`/`AuditSink` 为消息日志 + 审计；
//! 新增 `EventStore`/`SnapshotStore` 为 Event Sourcing（见 `design.md` §25）。

mod event;
mod replay;
mod snapshot;
mod r#trait;

pub use event::{
    EventRecord, EventStore, NoopEventStore, PersistedEvent, SCHEMA_VERSION, try_persist,
};
pub use replay::{ReplayError, ReplayedSession, replay_session_state, session_from_messages};
pub use snapshot::{
    NoopSnapshotStore, SNAPSHOT_INTERVAL, SessionSnapshot, SessionState, SnapshotStore,
};
pub use r#trait::{
    AuditKind, AuditRecord, AuditSink, NoopAudit, SessionMeta, Storage, StorageError,
};
