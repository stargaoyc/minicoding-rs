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
    /// 会话摘要（首条用户消息或 LLM 生成摘要；可能为空）。
    #[serde(default)]
    pub summary: Option<String>,
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
    /// 上下文压缩（M-07，R-02）：detail 携带压缩区间与掉 token 量，可追溯。
    Compress,
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

#[cfg(test)]
mod tests {
    //! `SessionMeta` / `AuditRecord` / `AuditKind` / `NoopAudit` 测试（覆盖率补全）。

    use super::*;
    use crate::model::SessionId;

    fn sample_meta() -> SessionMeta {
        SessionMeta {
            id: SessionId::from("01TEST"),
            created_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            message_count: 5,
            last_message_at: OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap(),
            summary: None,
        }
    }

    #[test]
    fn session_meta_serde_roundtrip() {
        let meta = sample_meta();
        let json = serde_json::to_string(&meta).expect("serialize");
        let decoded: SessionMeta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.id, meta.id);
        assert_eq!(decoded.message_count, meta.message_count);
    }

    #[test]
    fn audit_kind_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&AuditKind::PermissionRequested).unwrap(),
            "\"permission_requested\""
        );
        assert_eq!(
            serde_json::to_string(&AuditKind::PermissionResolved).unwrap(),
            "\"permission_resolved\""
        );
        assert_eq!(
            serde_json::to_string(&AuditKind::ToolCall).unwrap(),
            "\"tool_call\""
        );
        assert_eq!(
            serde_json::to_string(&AuditKind::ToolResult).unwrap(),
            "\"tool_result\""
        );
        assert_eq!(
            serde_json::to_string(&AuditKind::HookRun).unwrap(),
            "\"hook_run\""
        );
        assert_eq!(
            serde_json::to_string(&AuditKind::FileUndone).unwrap(),
            "\"file_undone\""
        );
    }

    #[test]
    fn audit_record_serde_roundtrip() {
        let rec = AuditRecord {
            ts: OffsetDateTime::from_unix_timestamp(1_700_000_050).unwrap(),
            session: SessionId::from("01AUDIT"),
            kind: AuditKind::ToolCall,
            tool: Some("shell.run".to_string()),
            decision: None,
            detail: "executed echo".to_string(),
        };
        let json = serde_json::to_string(&rec).expect("serialize");
        let decoded: AuditRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.session, rec.session);
        assert!(matches!(decoded.kind, AuditKind::ToolCall));
        assert_eq!(decoded.tool.as_deref(), Some("shell.run"));
        assert!(decoded.decision.is_none());
        assert_eq!(decoded.detail, "executed echo");
    }

    #[tokio::test]
    async fn noop_audit_record_returns_ok() {
        let sink = NoopAudit;
        let rec = AuditRecord {
            ts: OffsetDateTime::now_utc(),
            session: SessionId::from("01NOOP"),
            kind: AuditKind::PermissionResolved,
            tool: None,
            decision: Some("allow".to_string()),
            detail: "noop test".to_string(),
        };
        let result = sink.record(rec).await;
        assert!(result.is_ok());
    }
}
