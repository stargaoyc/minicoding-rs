//! Storage 模块 re-export。
//!
//! M0-M8：`Storage`/`AuditSink` 为消息日志 + 审计；
//! 新增 `EventStore`/`SnapshotStore` 为 Event Sourcing（见 `design.md` §25）。
//!
//! M-05：`replay_session_state`/`session_from_messages` 重建算法已下沉到
//! `minicoding-storage`（core 只留 trait 与数据结构，不保留领域算法）。

mod event;
mod snapshot;
mod r#trait;

pub use event::{
    EventRecord, EventStore, NoopEventStore, PersistedEvent, SCHEMA_VERSION, try_persist,
};
pub use snapshot::{
    NoopSnapshotStore, SNAPSHOT_INTERVAL, SessionSnapshot, SessionState, SnapshotStore,
};
pub use r#trait::{
    AuditKind, AuditRecord, AuditSink, NoopAudit, SessionMeta, Storage, StorageError,
};
