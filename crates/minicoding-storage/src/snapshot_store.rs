//! `JsonlSnapshotStore`：JSON 文件后端的 `SnapshotStore` 实现（见 `design.md` §25.3）。
//!
//! 每会话一文件 `{base_dir}/{session_id}.snapshot.json`，覆盖写（先写 `.tmp` 再
//! `rename`，原子）。`load` 读最新 snapshot；`save` 覆盖旧 snapshot。

use camino::Utf8PathBuf;
use minicoding_core::model::SessionId;
use minicoding_core::provider::BoxFuture;
use minicoding_core::storage::{SessionSnapshot, SnapshotStore, StorageError};
use tokio::io::AsyncWriteExt;

/// JSON 文件 snapshot 存储。
///
/// 文件布局：`{base_dir}/{session_id}.snapshot.json`，单文件覆盖写。
/// 原子写入：先写 `.tmp` 再 `rename`（同文件系统原子）。
pub struct JsonlSnapshotStore {
    base_dir: Utf8PathBuf,
}

impl JsonlSnapshotStore {
    /// 创建 snapshot 存储实例，若 `base_dir` 不存在则创建。
    #[must_use]
    pub fn new(base_dir: Utf8PathBuf) -> Self {
        let _ = std::fs::create_dir_all(base_dir.as_std_path());
        Self { base_dir }
    }

    fn snapshot_path(&self, session: &SessionId) -> Utf8PathBuf {
        self.base_dir.join(format!("{session}.snapshot.json"))
    }

    /// 唯一 tmp 路径（ST-3，2026-08-28 R5 收尾）：固定 `{session}.snapshot.json.tmp`
    /// 跨进程并发写会互相截断/rename 交错（A 写一半 B truncate，A rename 把 B 的
    /// 半写文件搬走）。加 pid + 单调计数使每个写者用独立 tmp 文件，rename 仍是
    /// 同文件系统原子操作。
    fn tmp_path(&self, session: &SessionId) -> Utf8PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.base_dir.join(format!(
            "{session}.snapshot.json.tmp-{}-{n}",
            std::process::id()
        ))
    }

    /// 同步加载 snapshot（`--replay` 启动期用，与 `JsonlStorage::load_messages_sync`
    /// 同语义）。
    ///
    /// ST-3（2026-08-28 R5 收尾）：Corrupted 不再向上抛 `RuntimeError::Storage`
    /// 阻断启动——snapshot 只是恢复加速缓存（消息主数据在 `.jsonl` 事件流），
    /// 损坏时回退 `None` 走事件重放（warn 记录），与"缓存可重建"语义一致。
    ///
    /// # Errors
    /// - `StorageError::Io`：读取失败（除 `NotFound`）。
    pub fn load_sync(&self, session: &SessionId) -> Result<Option<SessionSnapshot>, StorageError> {
        let path = self.snapshot_path(session);
        let content = match std::fs::read_to_string(path.as_std_path()) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_str::<SessionSnapshot>(&content) {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(e) => {
                tracing::warn!(
                    session = %session,
                    error = %e,
                    "snapshot 损坏，回退 None（事件重放兜底）"
                );
                Ok(None)
            }
        }
    }

    /// 同步保存 snapshot（覆盖旧）。
    ///
    /// # Errors
    /// - `StorageError::Io`：写入或 rename 失败；
    /// - `StorageError::Serialize`：序列化失败。
    pub fn save_sync(&self, snapshot: &SessionSnapshot) -> Result<(), StorageError> {
        let path = self.snapshot_path(&snapshot.session_id);
        let tmp: Utf8PathBuf = self.tmp_path(&snapshot.session_id);
        let json = serde_json::to_string_pretty(snapshot)
            .map_err(|e| StorageError::Serialize(e.to_string()))?;
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600); // S19/C-04
            }
            let mut file = opts.open(tmp.as_std_path())?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(tmp.as_std_path(), path.as_std_path())?;
        // 父目录 fsync（2026-08-23 审查 §10）：rename 元数据需随目录项落盘，
        // 崩溃极端情况否则可能丢失（正文 fsync 已有，此处补齐闭环）。
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent.as_std_path())
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// 同步删除 snapshot。
    ///
    /// # Errors
    /// 文件删除失败（除 `NotFound`）时返回错误。
    pub fn delete_sync(&self, session: &SessionId) -> Result<(), StorageError> {
        let path = self.snapshot_path(session);
        match std::fs::remove_file(path.as_std_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

impl SnapshotStore for JsonlSnapshotStore {
    fn load(
        &self,
        session: &SessionId,
    ) -> BoxFuture<'_, Result<Option<SessionSnapshot>, StorageError>> {
        let session = session.clone();
        Box::pin(async move { self.load_sync(&session) })
    }

    fn save(&self, snapshot: SessionSnapshot) -> BoxFuture<'_, Result<(), StorageError>> {
        Box::pin(async move {
            // 异步路径：直接 tokio::fs 写入，原子 rename 保证崩溃安全。
            let path = self.snapshot_path(&snapshot.session_id);
            let tmp: Utf8PathBuf = self.tmp_path(&snapshot.session_id);
            let json = serde_json::to_string_pretty(&snapshot)
                .map_err(|e| StorageError::Serialize(e.to_string()))?;
            let mut aopts = tokio::fs::OpenOptions::new();
            aopts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            aopts.mode(0o600); // S19/C-04（tokio 自带该方法）
            let mut file = aopts.open(&tmp).await?;
            file.write_all(json.as_bytes()).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&tmp, &path).await?;
            // ST-3：父目录 fsync 补齐（async 路径此前遗漏，sync 路径已有）
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::File::open(parent).await?.sync_all().await;
            }
            Ok(())
        })
    }

    fn delete(&self, session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        let session = session.clone();
        Box::pin(async move { self.delete_sync(&session) })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::model::Message;
    use minicoding_core::storage::{SNAPSHOT_INTERVAL, SessionState};
    use tempfile::tempdir;
    use time::OffsetDateTime;

    fn storage(dir: &tempfile::TempDir) -> JsonlSnapshotStore {
        JsonlSnapshotStore::new(dir.path().to_path_buf().try_into().unwrap())
    }

    fn make_snapshot(session_id: &str, seq: u64, msgs: Vec<&str>) -> SessionSnapshot {
        let state = SessionState {
            id: session_id.to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            workdir: "/tmp/proj".to_string(),
            config_hash: 12345,
            messages: msgs.iter().map(|t| Message::user_text(*t)).collect(),
            permission_mode: None,
            sandbox_preset: None,
        };
        SessionSnapshot::new(seq, state)
    }

    #[tokio::test]
    async fn save_then_load_roundtrip() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SNAP".to_string();
        let snap = make_snapshot(&id, 5, vec!["hello", "world"]);

        st.save(snap).await.unwrap();
        let loaded = st.load(&id).await.unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.seq, 5);
        assert_eq!(loaded.session_id, id);
        assert_eq!(loaded.state.messages.len(), 2);
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let result = st.load(&"01NOEXIST".to_string()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn save_overwrites_old_snapshot() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01OVERWRITE".to_string();

        st.save(make_snapshot(&id, 3, vec!["old"])).await.unwrap();
        st.save(make_snapshot(&id, 10, vec!["new1", "new2"]))
            .await
            .unwrap();

        let loaded = st.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded.seq, 10);
        assert_eq!(loaded.state.messages.len(), 2);
        assert_eq!(loaded.state.messages[0].text(), "new1");
    }

    #[tokio::test]
    async fn delete_removes_snapshot() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01DEL".to_string();

        st.save(make_snapshot(&id, 1, vec!["x"])).await.unwrap();
        st.delete(&id).await.unwrap();
        assert!(st.load(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn corrupted_snapshot_degrades_to_none() {
        // ST-3（2026-08-28 R5 收尾）：损坏的 snapshot 不再抛 Corrupted 阻断启动，
        // 回退 None 走事件重放兜底（warn 记录）。
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01CORRUPT".to_string();
        let path = st.snapshot_path(&id);
        tokio::fs::write(&path, "not-json").await.unwrap();
        let result = st.load(&id).await;
        assert!(
            matches!(result, Ok(None)),
            "损坏 snapshot 应回退 None，实际 {result:?}"
        );
    }

    #[test]
    fn corrupted_snapshot_sync_degrades_to_none() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01CORRUPTSYNC".to_string();
        let path = st.snapshot_path(&id);
        std::fs::write(path.as_std_path(), "not-json").unwrap();
        let result = st.load_sync(&id);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn sync_save_then_load_works() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SYNC".to_string();
        let snap = make_snapshot(&id, 7, vec!["a", "b", "c"]);

        st.save_sync(&snap).unwrap();
        let loaded = st.load_sync(&id).unwrap().unwrap();
        assert_eq!(loaded.seq, 7);
        assert_eq!(loaded.state.messages.len(), 3);
    }

    #[test]
    fn snapshot_interval_constant() {
        // 文档化常量值，确保不被意外修改
        assert_eq!(SNAPSHOT_INTERVAL, 50);
    }

    #[test]
    fn save_sync_atomic_no_tmp_leftover() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01ATOMIC".to_string();
        let snap = make_snapshot(&id, 1, vec!["x"]);

        st.save_sync(&snap).unwrap();
        let path = st.snapshot_path(&id);
        let tmp: Utf8PathBuf = path.with_extension("json.tmp");
        assert!(!tmp.exists());
        assert!(path.exists());
    }
}
