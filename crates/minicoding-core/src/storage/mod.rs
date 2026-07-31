//! Storage 模块 re-export。

mod r#trait;

pub use r#trait::{
    AuditKind, AuditRecord, AuditSink, NoopAudit, SessionMeta, Storage, StorageError,
};
