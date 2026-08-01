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
use minicoding_core::provider::BoxFuture;
use minicoding_core::storage::{SessionMeta, Storage, StorageError};
use std::sync::Mutex;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
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
            if let Some(idx) = guard.as_ref() {
                if !idx.is_empty() {
                    return Ok(idx.to_metas());
                }
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
            let created_at = first.created_at.format(&Rfc3339).unwrap_or_default();
            let updated_at = last.created_at.format(&Rfc3339).unwrap_or_default();
            index.add(SessionIndexEntry {
                session_id: stem.to_string(),
                summary,
                message_count: lines.len(),
                created_at,
                updated_at,
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
            let created_at = first.created_at.format(&Rfc3339).unwrap_or_default();
            let updated_at = last.created_at.format(&Rfc3339).unwrap_or_default();
            index.add(SessionIndexEntry {
                session_id: stem.to_string(),
                summary,
                message_count: lines.len(),
                created_at,
                updated_at,
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
                if let Some(idx) = guard.as_ref() {
                    if !idx.is_empty() {
                        return Ok(idx.to_metas());
                    }
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
}
