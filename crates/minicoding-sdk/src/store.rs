//! `InMemoryStorage`：SDK 默认内存存储实现（不写盘）。
//!
//! 实现 `core::storage::Storage` trait，所有数据存内存 `Mutex<HashMap>`。
//! 用于 SDK 嵌入场景的默认存储——调用方不需要持久化时无需配置。
//!
//! 如需持久化（崩溃恢复、跨会话恢复），调用方注入
//! `minicoding_storage::JsonlStorage`（JSONL 追加写，崩溃安全，见
//! `minicoding-storage` crate）。

use minicoding_core::model::{Message, SessionId, StorageError};
use minicoding_core::provider::BoxFuture;
use minicoding_core::storage::{SessionListItem, Storage};
use std::collections::HashMap;
use std::sync::Mutex;
use time::OffsetDateTime;

/// 内存存储（SDK 默认）。
///
/// `Mutex<HashMap<SessionId, Vec<Message>>>` 持有所有会话消息。
/// `SessionListItem` 在 `list_sessions` 时从 `Vec<Message>` 现算。
///
/// 线程安全：`Mutex` 保护内部映射，`Arc<InMemoryStorage>` 可多线程共享。
/// 不持久化：进程退出后数据丢失（SDK 默认行为；如需持久化用 `JsonlStorage`）。
/// A13：与 core tests/common 的测试版 `InMemoryStorage` 语义相同，
/// 生产持久化请用 `minicoding-storage::JsonlStorage`。
#[derive(Debug, Default)]
pub struct InMemoryStorage {
    sessions: Mutex<HashMap<SessionId, Vec<Message>>>,
}

impl InMemoryStorage {
    /// 创建空内存存储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for InMemoryStorage {
    fn append(
        &self,
        session: &SessionId,
        msg: &Message,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        let session = session.clone();
        let msg = msg.clone();
        Box::pin(async move {
            let mut guard = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.entry(session).or_default().push(msg);
            Ok(())
        })
    }

    fn load(&self, session: &SessionId) -> BoxFuture<'_, Result<Vec<Message>, StorageError>> {
        let session = session.clone();
        Box::pin(async move {
            let guard = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .get(&session)
                .cloned()
                .ok_or_else(|| StorageError::NotFound(session.clone()))
        })
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionListItem>, StorageError>> {
        Box::pin(async move {
            let guard = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let metas = guard
                .iter()
                .map(|(id, msgs)| {
                    let last = msgs
                        .last()
                        .map_or_else(OffsetDateTime::now_utc, |m| m.created_at);
                    let first = msgs
                        .first()
                        .map_or_else(OffsetDateTime::now_utc, |m| m.created_at);
                    SessionListItem {
                        id: id.clone(),
                        created_at: first,
                        message_count: msgs.len(),
                        last_message_at: last,
                        summary: None,
                    }
                })
                .collect();
            Ok(metas)
        })
    }

    fn delete(&self, session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        let session = session.clone();
        Box::pin(async move {
            let mut guard = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.remove(&session);
            Ok(())
        })
    }

    fn update_summary(
        &self,
        _session: &SessionId,
        _summary: &str,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        // 内存存储不维护摘要索引（无持久化需求）。
        Box::pin(async move { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[tokio::test]
    async fn append_and_load() {
        let storage = InMemoryStorage::new();
        let sid = "01JTEST".to_string();
        let msg = Message::user_text("hello");
        storage.append(&sid, &msg).await.unwrap();
        let loaded = storage.load(&sid).await.unwrap();
        assert_eq!(loaded.len(), 1);
    }

    #[tokio::test]
    async fn load_missing_session_errors() {
        let storage = InMemoryStorage::new();
        let result = storage.load(&"missing".to_string()).await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }

    #[tokio::test]
    async fn delete_removes_session() {
        let storage = InMemoryStorage::new();
        let sid = "01JTEST".to_string();
        let msg = Message::user_text("hello");
        storage.append(&sid, &msg).await.unwrap();
        storage.delete(&sid).await.unwrap();
        let result = storage.load(&sid).await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }

    #[tokio::test]
    async fn list_sessions_returns_metas() {
        let storage = InMemoryStorage::new();
        storage
            .append(&"s1".to_string(), &Message::user_text("a"))
            .await
            .unwrap();
        storage
            .append(&"s1".to_string(), &Message::user_text("b"))
            .await
            .unwrap();
        storage
            .append(&"s2".to_string(), &Message::user_text("c"))
            .await
            .unwrap();
        let metas = storage.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 2);
        let s1 = metas.iter().find(|m| m.id == "s1").unwrap();
        assert_eq!(s1.message_count, 2);
    }

    #[tokio::test]
    async fn update_summary_is_noop() {
        let storage = InMemoryStorage::new();
        let result = storage.update_summary(&"any".to_string(), "summary").await;
        assert!(result.is_ok());
    }
}
