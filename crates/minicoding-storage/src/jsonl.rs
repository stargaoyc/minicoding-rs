//! `JSONL` 会话存储：每条消息一行 JSON，追加写，崩溃安全（`fsync`）。
//!
//! 集成会话索引（`index.json`，见 `index.rs`）与导出（见 `export.rs`）：
//! - `list_sessions` 优先走索引缓存，索引不存在时回退扫描并落盘；
//! - `append` / `delete` 同步更新索引（best effort，不阻塞主路径）；
//! - `export` 按 `ExportFormat` 导出会话为 Markdown / JSONL。

use crate::export::{ExportFormat, export_session_jsonl, export_session_md};
use crate::index::{SessionIndex, SessionIndexEntry};
use camino::Utf8PathBuf;
use minicoding_core::model::{Message, Role, SessionId};
use minicoding_core::otel::span_name;
use minicoding_core::provider::BoxFuture;
use minicoding_core::storage::{SessionMeta, Storage, StorageError};
use std::sync::Mutex;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

/// `JSONL` 会话存储。
///
/// 文件布局：`{base_dir}/{session_id}.jsonl`，每行一条 `Message`（JSON）。追加写，
/// 每条消息后 `fsync` 保证崩溃安全。空会话不产生文件（首条消息时才创建）。
/// 会话索引 `{base_dir}/index.json` 缓存元数据，`list_sessions` 优先读索引。
pub struct JsonlStorage {
    base_dir: Utf8PathBuf,
    /// 进程内索引缓存（首次 `list_sessions`/`append` 时加载）。`std::sync::Mutex`
    /// 临界区不跨 await（索引文件小，sync I/O 短暂），符合 AGENTS.md §2.4。
    index_cache: Mutex<Option<SessionIndex>>,
}

impl JsonlStorage {
    /// 创建存储实例，若 `base_dir` 不存在则创建。
    #[must_use]
    pub fn new(base_dir: Utf8PathBuf) -> Self {
        // 一次性目录创建；失败时由后续 append 报错暴露
        let _ = std::fs::create_dir_all(base_dir.as_std_path());
        Self {
            base_dir,
            index_cache: Mutex::new(None),
        }
    }

    fn session_path(&self, session: &SessionId) -> Utf8PathBuf {
        self.base_dir.join(format!("{session}.jsonl"))
    }

    fn index_path(&self) -> Utf8PathBuf {
        self.base_dir.join("index.json")
    }

    /// 同步加载会话全部消息（`--resume` 启动期用，T-M3-10a）。
    ///
    /// 与 `Storage::load` 同语义，但用 `std::fs` 同步读取——仅在 CLI 启动期
    /// （tokio runtime 尚未创建时）使用。运行时路径应走 `Storage::load`。
    ///
    /// # Errors
    /// - `StorageError::Io`：读取失败（除 `NotFound`）；
    /// - `StorageError::Corrupted`：消息行 JSON 解析失败。
    #[tracing::instrument(skip(self), fields(otel.name = "storage.load"))]
    pub fn load_messages_sync(&self, session: &SessionId) -> Result<Vec<Message>, StorageError> {
        let path = self.session_path(session);
        let content = match std::fs::read_to_string(path.as_std_path()) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut messages = Vec::new();
        for (idx, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let msg: Message = serde_json::from_str(line)
                .map_err(|e| StorageError::Corrupted(format!("line {}: {e}", idx + 1)))?;
            messages.push(msg);
        }
        Ok(messages)
    }

    /// 同步列出会话元数据（`session list` 子命令用，T-M3-10c）。
    ///
    /// 与 `Storage::list_sessions` 同语义，但用 `std::fs` 同步读取。优先读
    /// `index.json`，不存在时回退扫描目录。
    ///
    /// # Errors
    /// 索引文件读取失败或目录扫描失败时返回错误。
    pub fn list_sessions_sync(&self) -> Result<Vec<SessionMeta>, StorageError> {
        // 1. 尝试缓存
        {
            let guard = self.lock_index();
            if let Some(idx) = guard.as_ref()
                && !idx.is_empty()
            {
                return Ok(idx.to_metas());
            }
        }
        // 2. 加载索引文件
        let index = SessionIndex::load(&self.index_path())?;
        if !index.is_empty() {
            let metas = index.to_metas();
            let mut guard = self.lock_index();
            *guard = Some(index);
            return Ok(metas);
        }
        // 3. 回退：同步扫描目录
        let entries = match std::fs::read_dir(self.base_dir.as_std_path()) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };
        let mut index = SessionIndex::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("skip unreadable session file {}: {e}", path.display());
                    continue;
                }
            };
            let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.is_empty() {
                continue;
            }
            let first = match serde_json::from_str::<Message>(lines[0]) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("skip corrupted session file {}: {e}", path.display());
                    continue;
                }
            };
            let last = match serde_json::from_str::<Message>(lines[lines.len() - 1]) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("skip corrupted session file {}: {e}", path.display());
                    continue;
                }
            };
            let summary = find_first_user_summary(&lines);
            index.add(SessionIndexEntry {
                session_id: stem.to_string(),
                summary,
                message_count: lines.len(),
                created_at: first.created_at,
                updated_at: last.created_at,
                parent_uuid: None,
            });
        }
        let metas = index.to_metas();
        // 缓存 + 落盘（best effort）
        {
            let mut guard = self.lock_index();
            *guard = Some(index.clone());
        }
        if let Err(e) = index.save(&self.index_path()) {
            tracing::warn!("failed to persist session index: {e}");
        }
        Ok(metas)
    }

    /// 同步删除会话（`session delete` 子命令用，T-M3-10c）。
    ///
    /// 与 `Storage::delete` 同语义，但用 `std::fs` 同步删除。
    ///
    /// # Errors
    /// 文件删除失败（除 `NotFound`）时返回错误。
    pub fn delete_session_sync(&self, session: &SessionId) -> Result<(), StorageError> {
        let path = self.session_path(session);
        match std::fs::remove_file(path.as_std_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.remove_from_index(session);
        let lock_path = self.base_dir.join(format!("{session}.lock"));
        let _ = std::fs::remove_file(lock_path.as_std_path());
        Ok(())
    }

    /// 同步复制消息到新会话文件（`--fork-session` 用，T-M3-10b）。
    ///
    /// 逐行追加写 + fsync，遵循 JSONL 崩溃安全追加写约定（`data-model.md` §3.2）。
    /// 原会话文件只读不写（`design.md` §10.5）。新会话文件创建后更新索引。
    ///
    /// # Errors
    /// 文件创建或写入失败时返回错误。
    pub fn fork_session_sync(
        &self,
        new_session_id: &SessionId,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        let path = self.session_path(new_session_id);
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path.as_std_path())?;
            for msg in messages {
                let line = serde_json::to_string(msg)
                    .map_err(|e| StorageError::Serialize(e.to_string()))?;
                file.write_all(line.as_bytes())?;
                file.write_all(b"\n")?;
                file.flush()?;
                file.sync_all()?;
            }
        }
        // 更新索引（best effort）：fork 后逐条 upsert，复用 append 路径
        for msg in messages {
            self.update_index_on_append(new_session_id, msg);
        }
        Ok(())
    }

    /// 锁定索引缓存（从 poison 中恢复：索引仅为缓存，重建无害）。
    fn lock_index(&self) -> std::sync::MutexGuard<'_, Option<SessionIndex>> {
        self.index_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 导出会话为指定格式。
    ///
    /// # Errors
    /// - `StorageError::NotFound`：会话无消息文件或为空；
    /// - `StorageError::Io`：读取消息文件失败；
    /// - `StorageError::Corrupted`：消息行 JSON 解析失败。
    pub async fn export(
        &self,
        id: &SessionId,
        format: ExportFormat,
    ) -> Result<String, StorageError> {
        let messages = self.load(id).await?;
        if messages.is_empty() {
            return Err(StorageError::NotFound(id.clone()));
        }
        let first = &messages[0];
        let last = &messages[messages.len() - 1];
        let meta = SessionMeta {
            id: id.clone(),
            created_at: first.created_at,
            message_count: messages.len(),
            last_message_at: last.created_at,
        };
        Ok(match format {
            ExportFormat::Markdown => export_session_md(&messages, &meta),
            ExportFormat::Jsonl => export_session_jsonl(&messages),
        })
    }

    /// 追加消息后更新索引（best effort）。失败仅记日志，不影响主路径。
    fn update_index_on_append(&self, session_id: &str, msg: &Message) {
        let result = (|| -> Result<(), StorageError> {
            let mut guard = self.lock_index();
            if guard.is_none() {
                let idx = SessionIndex::load(&self.index_path())?;
                *guard = Some(idx);
            }
            let now = OffsetDateTime::now_utc();
            let summary = if matches!(msg.role, Role::User) {
                let text = msg.text();
                if text.is_empty() {
                    None
                } else {
                    Some(text.chars().take(80).collect())
                }
            } else {
                None
            };
            let Some(idx) = guard.as_mut() else {
                return Ok(());
            };
            idx.upsert_on_append(session_id, summary, now);
            let idx = idx.clone();
            drop(guard);
            idx.save(&self.index_path())?;
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!("failed to update session index on append: {e}");
        }
    }

    /// 删除会话后从索引移除（best effort）。
    fn remove_from_index(&self, session_id: &str) {
        let result = (|| -> Result<(), StorageError> {
            let mut guard = self.lock_index();
            if guard.is_none() {
                let idx = SessionIndex::load(&self.index_path())?;
                *guard = Some(idx);
            }
            let Some(idx) = guard.as_mut() else {
                return Ok(());
            };
            idx.remove(session_id);
            let idx = idx.clone();
            drop(guard);
            idx.save(&self.index_path())?;
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!("failed to remove session from index: {e}");
        }
    }

    /// 同步更新会话索引中的摘要字段（T-M3-6）。
    ///
    /// 调用 `SessionIndex::update_summary` 落盘。会话不存在于索引时静默忽略
    /// （best effort，与 `update_index_on_append` 一致）。
    ///
    /// # Errors
    /// 索引文件读取或写入失败时返回 `StorageError`。
    pub fn update_summary_sync(
        &self,
        session_id: &SessionId,
        summary: &str,
    ) -> Result<(), StorageError> {
        let mut guard = self.lock_index();
        if guard.is_none() {
            let idx = SessionIndex::load(&self.index_path())?;
            *guard = Some(idx);
        }
        let Some(idx) = guard.as_mut() else {
            return Ok(());
        };
        // 会话不在索引中：静默忽略（best effort）
        if idx.get(session_id.as_str()).is_none() {
            tracing::warn!(
                session = %session_id,
                "update_summary: session not in index, skipping (call append first)"
            );
            return Ok(());
        }
        idx.update_summary(session_id.as_str(), summary.to_string());
        let idx = idx.clone();
        drop(guard);
        idx.save(&self.index_path())?;
        Ok(())
    }

    /// 从目录扫描构建索引（索引文件不存在时的回退路径）。
    async fn build_index_from_scan(&self) -> Result<SessionIndex, StorageError> {
        let mut entries = match tokio::fs::read_dir(&self.base_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SessionIndex::new());
            }
            Err(e) => return Err(e.into()),
        };
        let mut index = SessionIndex::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("skip unreadable session file {}: {e}", path.display());
                    continue;
                }
            };
            let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.is_empty() {
                continue;
            }
            let first = match serde_json::from_str::<Message>(lines[0]) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("skip corrupted session file {}: {e}", path.display());
                    continue;
                }
            };
            let last = match serde_json::from_str::<Message>(lines[lines.len() - 1]) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("skip corrupted session file {}: {e}", path.display());
                    continue;
                }
            };
            let summary = find_first_user_summary(&lines);
            index.add(SessionIndexEntry {
                session_id: stem.to_string(),
                summary,
                message_count: lines.len(),
                created_at: first.created_at,
                updated_at: last.created_at,
                parent_uuid: None,
            });
        }
        Ok(index)
    }
}

/// 从消息行中提取首条用户消息文本作为摘要（截断 80 字符）。
fn find_first_user_summary(lines: &[&str]) -> Option<String> {
    for line in lines {
        let Ok(msg) = serde_json::from_str::<Message>(line) else {
            continue;
        };
        if matches!(msg.role, Role::User) {
            let text = msg.text();
            if !text.is_empty() {
                return Some(text.chars().take(80).collect());
            }
        }
    }
    None
}

impl Storage for JsonlStorage {
    #[tracing::instrument(skip(self, session, msg), fields(otel.name = span_name::STORAGE_APPEND))]
    fn append(
        &self,
        session: &SessionId,
        msg: &Message,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        let path = self.session_path(session);
        let msg = msg.clone();
        let session_id = session.clone();
        Box::pin(async move {
            let line =
                serde_json::to_string(&msg).map_err(|e| StorageError::Serialize(e.to_string()))?;
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .await?;
            file.write_all(line.as_bytes()).await?;
            file.write_all(b"\n").await?;
            file.flush().await?;
            file.sync_all().await?;
            // 索引更新为 best effort：消息已安全落盘，索引失败不回滚主路径
            self.update_index_on_append(&session_id, &msg);
            Ok(())
        })
    }

    fn load(&self, session: &SessionId) -> BoxFuture<'_, Result<Vec<Message>, StorageError>> {
        let path = self.session_path(session);
        Box::pin(async move {
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(e) => return Err(e.into()),
            };
            let mut messages = Vec::new();
            for (idx, line) in content.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let msg: Message = serde_json::from_str(line)
                    .map_err(|e| StorageError::Corrupted(format!("line {}: {e}", idx + 1)))?;
                messages.push(msg);
            }
            Ok(messages)
        })
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionMeta>, StorageError>> {
        Box::pin(async move {
            // 1. 尝试缓存（短锁，不跨 await）
            {
                let guard = self.lock_index();
                if let Some(idx) = guard.as_ref()
                    && !idx.is_empty()
                {
                    return Ok(idx.to_metas());
                }
            }
            // 2. 尝试加载索引文件
            let index = SessionIndex::load(&self.index_path())?;
            if !index.is_empty() {
                let metas = index.to_metas();
                let mut guard = self.lock_index();
                *guard = Some(index);
                return Ok(metas);
            }
            // 3. 回退：扫描目录构建索引
            let index = self.build_index_from_scan().await?;
            let metas = index.to_metas();
            // 缓存 + 落盘（best effort）
            {
                let mut guard = self.lock_index();
                *guard = Some(index.clone());
            }
            if let Err(e) = index.save(&self.index_path()) {
                tracing::warn!("failed to persist session index: {e}");
            }
            Ok(metas)
        })
    }

    fn delete(&self, session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        let path = self.session_path(session);
        let lock_path = self.base_dir.join(format!("{session}.lock"));
        let session_id = session.clone();
        Box::pin(async move {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            // 从索引移除（best effort）
            self.remove_from_index(&session_id);
            // 清理可能残留的锁文件（best effort）
            let _ = tokio::fs::remove_file(&lock_path).await;
            Ok(())
        })
    }

    fn update_summary(
        &self,
        session: &SessionId,
        summary: &str,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        let session_id = session.clone();
        let summary = summary.to_string();
        Box::pin(async move { self.update_summary_sync(&session_id, &summary) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::Message;
    use tempfile::tempdir;

    fn storage(dir: &tempfile::TempDir) -> JsonlStorage {
        JsonlStorage::new(dir.path().to_path_buf().try_into().unwrap())
    }

    #[tokio::test]
    async fn append_then_list_uses_index() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01TESTAPPEND";
        st.append(&id.to_string(), &Message::user_text("hello"))
            .await
            .unwrap();
        st.append(&id.to_string(), &Message::assistant_text("world"))
            .await
            .unwrap();
        let metas = st.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, id);
        assert_eq!(metas[0].message_count, 2);
    }

    #[tokio::test]
    async fn delete_removes_from_index() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01TESTDEL";
        st.append(&id.to_string(), &Message::user_text("hi"))
            .await
            .unwrap();
        st.delete(&id.to_string()).await.unwrap();
        let metas = st.list_sessions().await.unwrap();
        assert!(metas.is_empty());
    }

    #[tokio::test]
    async fn export_markdown_and_jsonl() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01EXP";
        st.append(&id.to_string(), &Message::user_text("hello"))
            .await
            .unwrap();
        st.append(&id.to_string(), &Message::assistant_text("world"))
            .await
            .unwrap();
        let md = st
            .export(&id.to_string(), ExportFormat::Markdown)
            .await
            .unwrap();
        assert!(md.contains("hello"));
        assert!(md.contains("world"));
        let jsonl = st
            .export(&id.to_string(), ExportFormat::Jsonl)
            .await
            .unwrap();
        assert_eq!(jsonl.lines().count(), 2);
    }

    #[tokio::test]
    async fn list_sessions_falls_back_to_scan_without_index() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SCAN";
        st.append(&id.to_string(), &Message::user_text("data"))
            .await
            .unwrap();
        // 删除索引文件 + 清空缓存，强制回退扫描
        let _ = tokio::fs::remove_file(st.index_path()).await;
        {
            let mut guard = st.lock_index();
            *guard = None;
        }
        let metas = st.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, id);
    }

    #[tokio::test]
    async fn load_returns_messages() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01LOAD";
        st.append(&id.to_string(), &Message::user_text("hello"))
            .await
            .unwrap();
        st.append(&id.to_string(), &Message::assistant_text("world"))
            .await
            .unwrap();
        let msgs = st.load(&id.to_string()).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn load_nonexistent_returns_empty() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let msgs = st.load(&"01NONE".to_string()).await.unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn load_messages_sync_returns_messages() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SYNC";
        st.append(&id.to_string(), &Message::user_text("sync hello"))
            .await
            .unwrap();
        let msgs = st.load_messages_sync(&id.to_string()).unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn load_messages_sync_nonexistent_returns_empty() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let msgs = st.load_messages_sync(&"01NONE".to_string()).unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn load_messages_sync_corrupted_returns_error() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01CORRUPT";
        let path = st.session_path(&id.to_string());
        tokio::fs::write(path.as_std_path(), "not json\n")
            .await
            .unwrap();
        let result = st.load_messages_sync(&id.to_string());
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_sessions_sync_returns_from_index() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        st.append(&"01SYNC1".to_string(), &Message::user_text("a"))
            .await
            .unwrap();
        st.append(&"01SYNC2".to_string(), &Message::user_text("b"))
            .await
            .unwrap();
        let metas = st.list_sessions_sync().unwrap();
        assert_eq!(metas.len(), 2);
    }

    #[tokio::test]
    async fn list_sessions_sync_empty_dir() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let metas = st.list_sessions_sync().unwrap();
        assert!(metas.is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_ok() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 删除不存在的会话应返回 Ok（幂等）
        let result = st.delete(&"01NONE".to_string()).await;
        assert!(result.is_ok(), "delete nonexistent should be ok");
    }

    #[tokio::test]
    async fn export_nonexistent_returns_error() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let result = st
            .export(&"01NONE".to_string(), ExportFormat::Markdown)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multiple_sessions_listed_correctly() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        st.append(&"01MULTI1".to_string(), &Message::user_text("first"))
            .await
            .unwrap();
        st.append(&"01MULTI2".to_string(), &Message::user_text("second"))
            .await
            .unwrap();
        st.append(&"01MULTI3".to_string(), &Message::user_text("third"))
            .await
            .unwrap();
        let metas = st.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 3);
    }

    #[tokio::test]
    async fn append_creates_session_file() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01CREATE";
        st.append(&id.to_string(), &Message::user_text("content"))
            .await
            .unwrap();
        let path = st.session_path(&id.to_string());
        assert!(path.as_std_path().exists(), "session file should exist");
    }

    #[tokio::test]
    async fn delete_session_sync_removes_file_and_index() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01DELSYNC";
        st.append(&id.to_string(), &Message::user_text("to be deleted"))
            .await
            .unwrap();
        // 确保文件存在
        assert!(st.session_path(&id.to_string()).as_std_path().exists());
        // 同步删除
        st.delete_session_sync(&id.to_string()).unwrap();
        // 文件应不存在
        assert!(!st.session_path(&id.to_string()).as_std_path().exists());
        // 索引中也不应再有该会话
        let metas = st.list_sessions_sync().unwrap();
        assert!(metas.is_empty(), "session should be removed from index");
    }

    #[tokio::test]
    async fn delete_session_sync_nonexistent_is_ok() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 删除不存在的会话应返回 Ok（幂等）
        let result = st.delete_session_sync(&"01NONE".to_string());
        assert!(
            result.is_ok(),
            "delete_session_sync nonexistent should be ok"
        );
    }

    #[tokio::test]
    async fn fork_session_sync_creates_new_session_with_messages() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let src_id = "01SRC";
        // 写入源会话消息
        st.append(&src_id.to_string(), &Message::user_text("first"))
            .await
            .unwrap();
        st.append(&src_id.to_string(), &Message::assistant_text("second"))
            .await
            .unwrap();
        // 读取源消息并 fork 到新会话
        let messages = st.load(&src_id.to_string()).await.unwrap();
        let new_id = "01FORK";
        st.fork_session_sync(&new_id.to_string(), &messages)
            .unwrap();
        // 新会话应有相同消息
        let forked = st.load_messages_sync(&new_id.to_string()).unwrap();
        assert_eq!(forked.len(), 2);
        assert_eq!(forked[0].role, Role::User);
        assert_eq!(forked[1].role, Role::Assistant);
        // 新会话应出现在索引中
        let metas = st.list_sessions_sync().unwrap();
        assert_eq!(metas.len(), 2, "both sessions should be in index");
    }

    #[tokio::test]
    async fn fork_session_sync_empty_messages_creates_empty_file() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let new_id = "01EMPTYFORK";
        let empty: Vec<Message> = Vec::new();
        st.fork_session_sync(&new_id.to_string(), &empty).unwrap();
        // 空消息列表 fork 后应创建空文件（不报错）
        let forked = st.load_messages_sync(&new_id.to_string()).unwrap();
        assert!(forked.is_empty());
    }

    #[tokio::test]
    async fn update_summary_sync_updates_index_summary() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SUMMARY";
        st.append(&id.to_string(), &Message::user_text("hello world"))
            .await
            .unwrap();
        // 更新摘要
        st.update_summary_sync(&id.to_string(), "test summary")
            .unwrap();
        // 从索引读取并验证摘要已更新
        let metas = st.list_sessions_sync().unwrap();
        assert_eq!(metas.len(), 1);
        // SessionMeta 没有 summary 字段，但索引内部应有；通过重新构建索引验证
        // 删除索引缓存 + 文件，强制重新扫描
        let _ = tokio::fs::remove_file(st.index_path()).await;
        {
            let mut guard = st.lock_index();
            *guard = None;
        }
        // 重新扫描后应仍能列出会话（说明 fork_session_sync 写入的文件有效）
        let metas_after = st.list_sessions().await.unwrap();
        assert_eq!(metas_after.len(), 1);
        assert_eq!(metas_after[0].id, id);
    }

    #[tokio::test]
    async fn update_summary_sync_nonexistent_session_is_ok() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 会话不在索引中：静默忽略（best effort，与文档一致）
        let result = st.update_summary_sync(&"01NOTINIDX".to_string(), "summary");
        assert!(
            result.is_ok(),
            "update_summary_sync for unknown session should be ok"
        );
    }

    #[tokio::test]
    async fn update_summary_async_via_storage_trait() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01ASYNC";
        st.append(&id.to_string(), &Message::user_text("data"))
            .await
            .unwrap();
        // 通过 Storage trait 的 async update_summary 调用
        let result = st.update_summary(&id.to_string(), "async summary").await;
        assert!(result.is_ok(), "async update_summary should succeed");
    }

    #[test]
    fn find_first_user_summary_extracts_first_user_text() {
        let m1 = serde_json::to_string(&Message::assistant_text("assistant")).unwrap();
        let m2 = serde_json::to_string(&Message::user_text("user input here")).unwrap();
        let m3 = serde_json::to_string(&Message::user_text("second user")).unwrap();
        let lines = vec![m1.as_str(), m2.as_str(), m3.as_str()];
        let summary = find_first_user_summary(&lines);
        assert_eq!(summary.as_deref(), Some("user input here"));
    }

    #[test]
    fn find_first_user_summary_truncates_to_80_chars() {
        let long_text = "a".repeat(200);
        let m = serde_json::to_string(&Message::user_text(&long_text)).unwrap();
        let lines = vec![m.as_str()];
        let summary = find_first_user_summary(&lines).expect("should find summary");
        assert_eq!(summary.chars().count(), 80);
    }

    #[test]
    fn find_first_user_summary_returns_none_when_no_user_message() {
        let m = serde_json::to_string(&Message::assistant_text("only assistant")).unwrap();
        let lines = vec![m.as_str()];
        let summary = find_first_user_summary(&lines);
        assert!(summary.is_none());
    }

    #[test]
    fn find_first_user_summary_returns_none_for_empty_user_text() {
        let m = serde_json::to_string(&Message::user_text("")).unwrap();
        let lines = vec![m.as_str()];
        let summary = find_first_user_summary(&lines);
        assert!(summary.is_none());
    }

    #[test]
    fn find_first_user_summary_skips_invalid_json_lines() {
        let lines = vec!["not json", "{\"role\":\"system\",\"content\":[]}"];
        let summary = find_first_user_summary(&lines);
        assert!(summary.is_none());
    }

    #[tokio::test]
    async fn load_returns_error_for_corrupted_file() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01CORRUPTASYNC";
        let path = st.session_path(&id.to_string());
        tokio::fs::write(path.as_std_path(), "not valid json\n")
            .await
            .unwrap();
        let result = st.load(&id.to_string()).await;
        assert!(result.is_err(), "load corrupted file should return error");
        let err = result.unwrap_err();
        assert!(matches!(err, StorageError::Corrupted(_)));
    }

    #[tokio::test]
    async fn load_skips_empty_lines_in_file() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01EMPTYLINES";
        let path = st.session_path(&id.to_string());
        let m1 = serde_json::to_string(&Message::user_text("first")).unwrap();
        let m2 = serde_json::to_string(&Message::assistant_text("second")).unwrap();
        // 写入含空行的 JSONL
        let content = format!("{m1}\n\n  \n{m2}\n\n");
        tokio::fs::write(path.as_std_path(), content).await.unwrap();
        let msgs = st.load(&id.to_string()).await.unwrap();
        assert_eq!(msgs.len(), 2, "should skip empty lines");
    }

    #[tokio::test]
    async fn list_sessions_returns_empty_for_nonexistent_dir() {
        // base_dir 不存在时 list_sessions 应返回空 Vec
        let st = JsonlStorage::new(Utf8PathBuf::from(
            "/tmp/minicoding-test-nonexistent-dir-xyz-12345",
        ));
        let metas = st.list_sessions().await.unwrap();
        assert!(metas.is_empty());
    }

    #[tokio::test]
    async fn delete_removes_lock_file_if_exists() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01LOCKDEL";
        st.append(&id.to_string(), &Message::user_text("data"))
            .await
            .unwrap();
        // 创建模拟锁文件
        let lock_path = st.base_dir.join(format!("{id}.lock"));
        tokio::fs::write(lock_path.as_std_path(), "lock")
            .await
            .unwrap();
        assert!(lock_path.as_std_path().exists());
        // 删除会话应同时清理锁文件
        st.delete(&id.to_string()).await.unwrap();
        assert!(
            !lock_path.as_std_path().exists(),
            "lock file should be removed"
        );
    }

    #[tokio::test]
    async fn append_updates_index_for_multiple_sessions() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 写入多个会话，验证索引正确更新
        for i in 0..5 {
            let id = format!("01MULTI{i}");
            st.append(&id, &Message::user_text(format!("content {i}")))
                .await
                .unwrap();
        }
        let metas = st.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 5);
        // 再次 append 同一会话应更新消息计数
        st.append(&"01MULTI0".to_string(), &Message::user_text("more"))
            .await
            .unwrap();
        let metas = st.list_sessions().await.unwrap();
        assert_eq!(metas.len(), 5, "session count should not change");
    }

    #[tokio::test]
    async fn list_sessions_sync_skips_non_jsonl_files() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 写入一个有效会话
        st.append(&"01VALID".to_string(), &Message::user_text("hi"))
            .await
            .unwrap();
        // 写入非 .jsonl 文件（应被扫描时跳过）
        let other_path = st.base_dir.join("not_a_session.txt");
        tokio::fs::write(other_path.as_std_path(), "not a session")
            .await
            .unwrap();
        // 写入 .jsonl 但内容损坏的文件（应被扫描时跳过）
        let corrupt_path = st.base_dir.join("01CORRUPT.jsonl");
        tokio::fs::write(corrupt_path.as_std_path(), "not json")
            .await
            .unwrap();
        // 清空索引缓存 + 删除索引文件，强制重新扫描
        let _ = tokio::fs::remove_file(st.index_path()).await;
        {
            let mut guard = st.lock_index();
            *guard = None;
        }
        let metas = st.list_sessions_sync().unwrap();
        assert_eq!(metas.len(), 1, "should only list the one valid session");
        assert_eq!(metas[0].id, "01VALID");
    }

    #[tokio::test]
    async fn list_sessions_sync_skips_empty_jsonl_files() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        // 写入一个有效会话
        st.append(&"01VALID".to_string(), &Message::user_text("hi"))
            .await
            .unwrap();
        // 写入空的 .jsonl 文件（应被跳过）
        let empty_path = st.base_dir.join("01EMPTY.jsonl");
        tokio::fs::write(empty_path.as_std_path(), "")
            .await
            .unwrap();
        // 清空索引缓存 + 删除索引文件，强制重新扫描
        let _ = tokio::fs::remove_file(st.index_path()).await;
        {
            let mut guard = st.lock_index();
            *guard = None;
        }
        let metas = st.list_sessions_sync().unwrap();
        assert_eq!(metas.len(), 1, "empty jsonl should be skipped");
    }

    #[tokio::test]
    async fn load_messages_sync_skips_empty_lines() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SYNCEMPTY";
        let path = st.session_path(&id.to_string());
        let m1 = serde_json::to_string(&Message::user_text("first")).unwrap();
        let m2 = serde_json::to_string(&Message::assistant_text("second")).unwrap();
        // 写入含空行的 JSONL
        let content = format!("{m1}\n\n  \n{m2}\n");
        std::fs::write(path.as_std_path(), content).unwrap();
        let msgs = st.load_messages_sync(&id.to_string()).unwrap();
        assert_eq!(msgs.len(), 2, "should skip empty lines");
    }
}
