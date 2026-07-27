//! `JSONL` 会话存储：每条消息一行 JSON，追加写，崩溃安全（`fsync`）。

use camino::Utf8PathBuf;
use minicoding_core::model::{Message, SessionId};
use minicoding_core::provider::BoxFuture;
use minicoding_core::storage::{SessionMeta, Storage, StorageError};
use tokio::io::AsyncWriteExt;

/// `JSONL` 会话存储。
///
/// 文件布局：`{base_dir}/{session_id}.jsonl`，每行一条 `Message`（JSON）。追加写，
/// 每条消息后 `fsync` 保证崩溃安全。空会话不产生文件（首条消息时才创建）。
pub struct JsonlStorage {
    base_dir: Utf8PathBuf,
}

impl JsonlStorage {
    /// 创建存储实例，若 `base_dir` 不存在则创建。
    #[must_use]
    pub fn new(base_dir: Utf8PathBuf) -> Self {
        // 一次性目录创建；失败时由后续 append 报错暴露
        let _ = std::fs::create_dir_all(base_dir.as_std_path());
        Self { base_dir }
    }

    fn session_path(&self, session: &SessionId) -> Utf8PathBuf {
        self.base_dir.join(format!("{session}.jsonl"))
    }
}

impl Storage for JsonlStorage {
    fn append(
        &self,
        session: &SessionId,
        msg: &Message,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        let path = self.session_path(session);
        let msg = msg.clone();
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
        let base_dir = self.base_dir.clone();
        Box::pin(async move {
            let mut entries = match tokio::fs::read_dir(&base_dir).await {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(e) => return Err(e.into()),
            };
            let mut metas = Vec::new();
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
                metas.push(SessionMeta {
                    id: stem.to_string(),
                    created_at: first.created_at,
                    message_count: lines.len(),
                    last_message_at: last.created_at,
                });
            }
            Ok(metas)
        })
    }

    fn delete(&self, session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        let path = self.session_path(session);
        Box::pin(async move {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
    }
}
