//! 会话快照（snapshot）：周期性捕获 `Session` 全状态，加速事件重放。
//!
//! ## 设计要点
//!
//! - **触发条件**：每 N 条 `MessageAppended` 事件后落盘一次（默认 N=50），
//!   避免每次事件都全量 snapshot；
//! - **文件布局**：`{base_dir}/{session_id}.snapshot.json`，单文件覆盖写
//!   （snapshot 之间无版本关系，新 snapshot 覆盖旧）；
//! - **崩溃安全**：先写 `.tmp` 再 `rename`（同文件系统原子）；
//! - **重放路径**：`replay_session_state` 优先加载 snapshot，再应用 `seq >
//!   snapshot.seq` 的事件；
//! - **schema 版本化**：`SessionSnapshot.schema_version` 与 `EventRecord`
//!   共用 `SCHEMA_VERSION`，旧版 snapshot 通过 migration 适配。
//!
//! 详见 `design.md` §25.3、`data-model.md` §5.2。

use crate::model::{Message, SessionId};
use crate::provider::BoxFuture;
use crate::storage::StorageError;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 会话快照（捕获 `Session` 全状态）。
///
/// 用于 `replay_session_state` 加速：从 snapshot 加载初始状态，再应用 `seq >
/// snapshot.seq` 的事件，避免从空状态重放全部事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// 会话 ID。
    pub session_id: SessionId,
    /// 快照捕获的 seq（snapshot 包含此 seq 及之前所有事件的状态）。
    pub seq: u64,
    /// 快照生成时间（UTC）。
    #[serde(with = "time::serde::rfc3339")]
    pub taken_at: OffsetDateTime,
    /// snapshot schema 版本（与 `EventRecord::schema_version` 共用）。
    pub schema_version: u32,
    /// 快照状态。
    pub state: SessionState,
}

/// 会话状态（snapshot 的 payload）。
///
/// 与 `model::Session` 的字段一一对应，但独立定义以避免 `Session` 字段变更
/// 影响 snapshot 的反序列化（snapshot 需要 migration 兼容性，独立类型更安全）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// 会话 ID。
    pub id: SessionId,
    /// 创建时间（UTC）。
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// 工作目录。
    pub workdir: String,
    /// 配置 hash（resume 时校验一致性）。
    pub config_hash: u64,
    /// 消息列表（snapshot 时刻的完整快照）。
    pub messages: Vec<Message>,
    /// 会话安全上下文：快照时刻的 `PermissionMode`（serde `snake_case` 字符串，
    /// 如 `"plan"`/`"default"`）。FE-7（2026-08-25 R2 审查遗留）：此前不随快照
    /// 持久化，重启恢复后回落默认值，权限语义与中断前不一致。旧快照缺此字段
    /// 兼容读 `None`（serde default），调用方回落启动默认并告警。
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// 会话安全上下文：快照时刻的沙箱 preset 标识（`SandboxPolicy::preset_tag`，
    /// 如 `"workspace-write"`）。仅记录 preset 类别不含参数（workdir/writable 属
    /// 机器本地路径，跨机恢复无意义）；preset 变更是进程级启动决策，恢复侧只
    /// 对比告警不做热切换。旧快照兼容读 `None`。
    #[serde(default)]
    pub sandbox_preset: Option<String>,
}

impl SessionSnapshot {
    /// 构造快照（自动填入当前 schema 版本与时间戳）。
    #[must_use]
    pub fn new(seq: u64, state: SessionState) -> Self {
        Self {
            session_id: state.id.clone(),
            seq,
            taken_at: OffsetDateTime::now_utc(),
            schema_version: crate::storage::event::SCHEMA_VERSION,
            state,
        }
    }
}

/// 快照存储 trait（`dyn` 兼容）。
///
/// 实现见 `minicoding_storage::JsonlSnapshotStore`。
pub trait SnapshotStore: Send + Sync {
    /// 加载会话最近 snapshot；无 snapshot 时返回 `None`。
    ///
    /// # Errors
    /// - `StorageError::Io`：读取失败（除 `NotFound`）；
    /// - `StorageError::Corrupted`：JSON 解析失败。
    fn load(
        &self,
        session: &SessionId,
    ) -> BoxFuture<'_, Result<Option<SessionSnapshot>, StorageError>>;

    /// 保存 snapshot（覆盖旧 snapshot）。
    ///
    /// # Errors
    /// - `StorageError::Io`：写入或 rename 失败；
    /// - `StorageError::Serialize`：序列化失败。
    fn save(&self, snapshot: SessionSnapshot) -> BoxFuture<'_, Result<(), StorageError>>;

    /// 删除会话 snapshot。
    ///
    /// # Errors
    /// 文件删除失败（除 `NotFound`）时返回错误。
    fn delete(&self, session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>>;
}

/// 无操作 snapshot 存储（兜底，未启用 snapshot 时使用）。
pub struct NoopSnapshotStore;

impl SnapshotStore for NoopSnapshotStore {
    fn load(
        &self,
        _session: &SessionId,
    ) -> BoxFuture<'_, Result<Option<SessionSnapshot>, StorageError>> {
        Box::pin(async move { Ok(None) })
    }

    fn save(&self, _snapshot: SessionSnapshot) -> BoxFuture<'_, Result<(), StorageError>> {
        Box::pin(async move { Ok(()) })
    }

    fn delete(&self, _session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        Box::pin(async move { Ok(()) })
    }
}

/// 默认 snapshot 触发间隔（每 N 条 `MessageAppended` 事件触发一次）。
///
/// 取 50：长会话平均 200 条消息，4 个 snapshot 足以将 replay 时间从 O(N) 降到
/// O(N/4)；过小则 snapshot 写入开销大，过大则 replay 时间长。
pub const SNAPSHOT_INTERVAL: usize = 50;

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use crate::model::Message;

    #[test]
    fn snapshot_roundtrip() {
        let state = SessionState {
            id: "01TEST".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            workdir: "/tmp/proj".to_string(),
            config_hash: 12345,
            messages: vec![
                Message::user_text("hello"),
                Message::assistant_text("world"),
            ],
            permission_mode: Some("plan".to_string()),
            sandbox_preset: Some("workspace-write".to_string()),
        };
        let snap = SessionSnapshot::new(42, state);
        let json = serde_json::to_string(&snap).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 42);
        assert_eq!(back.session_id, "01TEST");
        assert_eq!(back.state.messages.len(), 2);
        assert_eq!(back.schema_version, crate::storage::event::SCHEMA_VERSION);
    }

    #[test]
    fn snapshot_security_context_roundtrip() {
        // FE-7：安全上下文字段双向序列化保真
        let state = SessionState {
            id: "01SEC".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            workdir: "/tmp".to_string(),
            config_hash: 0,
            messages: Vec::new(),
            permission_mode: Some("accept_edits".to_string()),
            sandbox_preset: Some("danger-full-access".to_string()),
        };
        let snap = SessionSnapshot::new(7, state);
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"permission_mode\":\"accept_edits\""));
        assert!(json.contains("\"sandbox_preset\":\"danger-full-access\""));
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state.permission_mode.as_deref(), Some("accept_edits"));
        assert_eq!(
            back.state.sandbox_preset.as_deref(),
            Some("danger-full-access")
        );
    }

    #[test]
    fn old_snapshot_without_security_fields_deserializes_none() {
        // 旧版（v3 前）快照 JSON 无 permission_mode/sandbox_preset 字段：
        // serde default 兼容读 None（向后兼容，恢复侧回落启动默认）
        let legacy_state = r#"{
            "id": "01LEGACY",
            "created_at": "1970-01-01T00:00:00Z",
            "workdir": "/tmp/proj",
            "config_hash": 0,
            "messages": []
        }"#;
        let state: SessionState = serde_json::from_str(legacy_state).unwrap();
        assert!(state.permission_mode.is_none());
        assert!(state.sandbox_preset.is_none());

        // 整个 snapshot 层同样兼容
        let legacy_snap = format!(
            r#"{{
                "session_id": "01LEGACY",
                "seq": 3,
                "taken_at": "1970-01-01T00:00:00Z",
                "schema_version": {legacy_version},
                "state": {legacy_state}
            }}"#,
            legacy_version = 2
        );
        let snap: SessionSnapshot = serde_json::from_str(&legacy_snap).unwrap();
        assert_eq!(snap.seq, 3);
        assert!(snap.state.permission_mode.is_none());
        assert!(snap.state.sandbox_preset.is_none());
    }

    #[tokio::test]
    async fn noop_snapshot_store_returns_none() {
        let store = NoopSnapshotStore;
        let result = store.load(&"01NOOP".to_string()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn noop_snapshot_store_save_is_noop() {
        let store = NoopSnapshotStore;
        let state = SessionState {
            id: "01NOOP".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            workdir: "/tmp".to_string(),
            config_hash: 0,
            messages: Vec::new(),
            permission_mode: None,
            sandbox_preset: None,
        };
        let snap = SessionSnapshot::new(1, state);
        store.save(snap).await.unwrap();
    }
}
