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
    /// 更新会话摘要（写入 `index.json`，T-M3-6）。
    ///
    /// 用于会话结束时 `SessionSummarizer` 生成摘要后落盘，供跨会话恢复与新
    /// 会话列出展示。会话不存在于索引时静默忽略（best effort，与 `append`
    /// 索引更新一致）。
    ///
    /// # Errors
    /// 索引文件读写失败时返回 `StorageError`。
    fn update_summary(
        &self,
        session: &SessionId,
        summary: &str,
    ) -> BoxFuture<'_, Result<(), StorageError>>;
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

/// 无操作审计 sink（兜底，未注入 audit 时使用）。
///
/// `record` 为空操作——仅用于测试或未启用审计的场景，真实落盘应由
/// `minicoding-storage::FileAuditSink` 提供（0600 权限，追加写）。
pub struct NoopAudit;

impl AuditSink for NoopAudit {
    fn record(&self, _rec: AuditRecord) -> BoxFuture<'_, Result<(), StorageError>> {
        Box::pin(async move { Ok(()) })
    }
}
