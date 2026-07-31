//! `Storage` / `AuditSink` trait（见 `api.md` §3.5）。
//!
//! 实现在 `minicoding-storage`（JSONL + audit.log）。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容。

use crate::model::{Message, SessionId};
use crate::provider::BoxFuture;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 会话元数据（轻量列出）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub created_at: OffsetDateTime,
    pub message_count: usize,
    pub last_message_at: OffsetDateTime,
}

/// 存储错误已在 `model::error` 定义，此处复用。
pub type StorageError = crate::model::StorageError;

/// 会话存储 trait（`dyn` 兼容）。
pub trait Storage: Send + Sync {
    fn append(&self, session: &SessionId, msg: &Message)
    -> BoxFuture<'_, Result<(), StorageError>>;
    fn load(&self, session: &SessionId) -> BoxFuture<'_, Result<Vec<Message>, StorageError>>;
    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionMeta>, StorageError>>;
    fn delete(&self, session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>>;
}

/// 审计事件记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub ts: OffsetDateTime,
    pub session: SessionId,
    pub kind: AuditKind,
    pub tool: Option<String>,
    pub decision: Option<String>,
    pub detail: String,
}

/// 审计事件类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    PermissionRequested,
    PermissionResolved,
    ToolCall,
    ToolResult,
    HookRun,
    FileUndone,
}

/// 审计 sink trait（权限决策等必须落盘，见 AGENTS.md §5.5，`dyn` 兼容）。
pub trait AuditSink: Send + Sync {
    fn record(&self, rec: AuditRecord) -> BoxFuture<'_, Result<(), StorageError>>;
}
