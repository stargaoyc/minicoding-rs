//! `JsonlEventStore`：JSONL 后端的 `EventStore` 实现（见 `design.md` §25.2）。
//!
//! 每会话一文件 `{base_dir}/{session_id}.events.jsonl`，每行一条 `EventRecord`。
//! 追加写 + `fsync` 保证崩溃安全（与 `JsonlStorage` 一致）。`load`/`load_after`
//! 按 seq 升序返回。`next_seq` 扫描文件最后一行的 seq + 1（O(1) 内存，O(N) IO，
//! 仅 Runtime 启动时调用一次）。

use camino::Utf8PathBuf;
use minicoding_core::model::SessionId;
use minicoding_core::provider::BoxFuture;
use minicoding_core::storage::{EventRecord, EventStore, StorageError};
use tokio::io::AsyncWriteExt;

/// JSONL 事件存储。
///
/// 文件布局：`{base_dir}/{session_id}.events.jsonl`，每行一条 `EventRecord`（JSON）。
/// 追加写，每条事件后 `fsync` 保证崩溃安全。空会话不产生文件（首条事件时才创建）。
pub struct JsonlEventStore {
    base_dir: Utf8PathBuf,
}

impl JsonlEventStore {
    /// 创建事件存储实例，若 `base_dir` 不存在则创建。
    #[must_use]
    pub fn new(base_dir: Utf8PathBuf) -> Self {
        let _ = std::fs::create_dir_all(base_dir.as_std_path());
        Self { base_dir }
    }

    fn session_path(&self, session: &SessionId) -> Utf8PathBuf {
        self.base_dir.join(format!("{session}.events.jsonl"))
    }

    /// 同步加载会话全部事件（`--replay` 启动期用，与 `JsonlStorage::load_messages_sync`
    /// 同语义）。
    ///
    /// # Errors
    /// - `StorageError::Io`：读取失败（除 `NotFound`）；
    /// - `StorageError::Corrupted`：事件行 JSON 解析失败。
    pub fn load_events_sync(&self, session: &SessionId) -> Result<Vec<EventRecord>, StorageError> {
        let path = self.session_path(session);
        let content = match std::fs::read_to_string(path.as_std_path()) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut records = Vec::new();
        for (idx, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record: EventRecord = serde_json::from_str(line)
                .map_err(|e| StorageError::Corrupted(format!("event line {}: {e}", idx + 1)))?;
            records.push(record);
        }
        Ok(records)
    }

    /// 同步返回下一个可分配的 seq（= 当前最大 seq + 1，空会话返回 1）。
    ///
    /// ST-9/ST-10（2026-08-28 R5 收尾）：改 seek 读文件尾块（O(1) 内存），
    /// 替代全文 `read_to_string`（此前长会话 append 每次全读——平方级 IO）；
    /// 且调用方须持 `{session}.lock` 再调本方法（ST-10 曾无锁读尾部，并发进程
    /// 可撞 seq；本方法不自行加锁，由 append/启动路径在锁内调用）。
    ///
    /// # Errors
    /// 读取失败时返回 `StorageError`。
    pub fn next_seq_sync(&self, session: &SessionId) -> Result<u64, StorageError> {
        let path = self.session_path(session);
        let tail = read_tail_line(path.as_std_path())
            .map_err(|e| StorageError::Io(std::io::Error::other(e.to_string())))?;
        let Some(line) = tail else {
            return Ok(1);
        };
        let record: EventRecord = serde_json::from_str(&line)
            .map_err(|e| StorageError::Corrupted(format!("last event line: {e}")))?;
        Ok(record.seq + 1)
    }

    /// 同步删除会话事件文件。
    ///
    /// # Errors
    /// 文件删除失败（除 `NotFound`）时返回错误。
    pub fn delete_events_sync(&self, session: &SessionId) -> Result<(), StorageError> {
        let path = self.session_path(session);
        match std::fs::remove_file(path.as_std_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// 读文件最后一行（非空，跳过尾部换行），O(1) 内存（ST-9，2026-08-28 R5 收尾）。
///
/// 实现：seek 到文件尾，反推 8 KiB 窗口内最后一行。**ST-R6-1（2026-08-28 R6
/// 审查）**：窗口起点若不在行首（最后一行 > 8 KiB 时窗口从行中部开始），片段
/// 必然非法 JSON——此前直接报 Corrupted，使 `append` 单调性检查恒失败（事件流
/// 冻结）且 `next_seq_sync` 失败（`--resume`/`--replay` 不可用）。修复：末行
/// 若跨窗口起点（缓冲区内无换行符且起点非文件头），向前回退窗口再读，直到
/// 取到完整末行；每次读 ≤ 8 KiB（O(1) 内存）。文件不存在/为空返回 `None`。
fn read_tail_line(path: &std::path::Path) -> std::io::Result<Option<String>> {
    use std::io::{Read, Seek, SeekFrom};
    const WINDOW: usize = 8 * 1024;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(None);
    }
    let mut start = len.saturating_sub(WINDOW as u64);
    loop {
        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::with_capacity(WINDOW);
        file.read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf);
        // 去尾部空白后找最后一个换行：换行后的内容即最后一行（必然完整，
        // 因为缓冲区读到 EOF）；无换行说明整块是同一条行的片段——起点不是
        // 文件头则回退扩展，起点是文件头则整块即末行。
        let trimmed = text.trim_end();
        if trimmed.is_empty() {
            if start == 0 {
                return Ok(None);
            }
            start = start.saturating_sub(WINDOW as u64);
            continue;
        }
        if let Some(nl_pos) = trimmed.rfind('\n') {
            return Ok(Some(trimmed[nl_pos + 1..].to_string()));
        }
        if start == 0 {
            return Ok(Some(trimmed.to_string()));
        }
        start = start.saturating_sub(WINDOW as u64);
    }
}

impl EventStore for JsonlEventStore {
    fn append(
        &self,
        session: &SessionId,
        record: EventRecord,
    ) -> BoxFuture<'_, Result<(), StorageError>> {
        let path = self.session_path(session);
        // M-01 同款串行化（2026-08-25 审查 §6.2-S8）：同会话事件流此前无锁，
        // 且行与换行分两次 write——跨进程并发 append 可交错出半行损坏。
        // 复用消息流的 `{session}.lock` 排他锁（顺带与消息 append 互斥）。
        let lock_path = self.base_dir.join(format!("{session}.lock"));
        Box::pin(async move {
            let line = serde_json::to_string(&record)
                .map_err(|e| StorageError::Serialize(e.to_string()))?;

            let _lock = tokio::task::spawn_blocking(move || {
                crate::lock::SessionLock::acquire_blocking(lock_path)
            })
            .await
            .map_err(|e| StorageError::Io(std::io::Error::other(e.to_string())))??;

            // SEC-9（2026-08-25 R2 审查）：seq 单调性校验。seq 由调用方（core
            // Runtime 的内存计数器）在**本进程**锁内分配，但两个独立进程同时
            // resume 同一会话时各自从文件尾播种计数器，会产出重复 seq——
            // load_after/SSE cursor 去重随之失效。锁内校验"新 seq 必须 > 文件
            // 尾 seq"，冲突 fail-closed 报错。
            // ST-9（2026-08-28 R5 收尾）：尾部读取改 seek 8 KiB 窗口（O(1) 内存），
            // 替代全文 read_to_string——长会话事件文件 append 每次全读为平方级 IO。
            {
                let tail = read_tail_line(path.as_std_path())
                    .map_err(|e| StorageError::Io(std::io::Error::other(e.to_string())))?;
                if let Some(last) = tail {
                    let last_record: EventRecord = serde_json::from_str(&last)
                        .map_err(|e| StorageError::Corrupted(format!("last event line: {e}")))?;
                    if record.seq <= last_record.seq {
                        return Err(StorageError::Corrupted(format!(
                            "event seq {} is not greater than persisted tail seq {} \
                             (concurrent writer on the same session?)",
                            record.seq, last_record.seq
                        )));
                    }
                }
            }

            // S19/C-04：事件流同样可能含敏感输出，0600 创建
            let mut opts = tokio::fs::OpenOptions::new();
            opts.append(true).create(true);
            #[cfg(unix)]
            opts.mode(0o600); // S19/C-04（tokio 自带该方法）
            let mut file = opts.open(&path).await?;
            #[cfg(unix)]
            crate::jsonl::tighten_existing(&file, &path).await;
            // 单次 write_all（行 + 换行）：原子追加，消除两 syscall 间交错窗口
            let mut buf = line.into_bytes();
            buf.push(b'\n');
            file.write_all(&buf).await?;
            file.flush().await?;
            file.sync_all().await?;
            Ok(())
        })
    }

    fn load(&self, session: &SessionId) -> BoxFuture<'_, Result<Vec<EventRecord>, StorageError>> {
        let path = self.session_path(session);
        Box::pin(async move {
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(e) => return Err(e.into()),
            };
            let mut records = Vec::new();
            for (idx, line) in content.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let record: EventRecord = serde_json::from_str(line)
                    .map_err(|e| StorageError::Corrupted(format!("event line {}: {e}", idx + 1)))?;
                records.push(record);
            }
            Ok(records)
        })
    }

    fn load_after(
        &self,
        session: &SessionId,
        after_seq: u64,
    ) -> BoxFuture<'_, Result<Vec<EventRecord>, StorageError>> {
        let path = self.session_path(session);
        Box::pin(async move {
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Vec::new());
                }
                Err(e) => return Err(e.into()),
            };
            let mut records = Vec::new();
            for (idx, line) in content.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let record: EventRecord = serde_json::from_str(line)
                    .map_err(|e| StorageError::Corrupted(format!("event line {}: {e}", idx + 1)))?;
                if record.seq > after_seq {
                    records.push(record);
                }
            }
            // 按 seq 升序（文件本身已是升序，无需排序）
            Ok(records)
        })
    }

    fn next_seq(&self, session: &SessionId) -> BoxFuture<'_, Result<u64, StorageError>> {
        let session = session.clone();
        Box::pin(async move { self.next_seq_sync(&session) })
    }

    fn delete(&self, session: &SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        let session = session.clone();
        Box::pin(async move { self.delete_events_sync(&session) })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::model::Message;
    use minicoding_core::storage::PersistedEvent;
    use tempfile::tempdir;

    fn storage(dir: &tempfile::TempDir) -> JsonlEventStore {
        JsonlEventStore::new(dir.path().to_path_buf().try_into().unwrap())
    }

    fn make_record(seq: u64, session: &str, text: &str) -> EventRecord {
        EventRecord::new(
            seq,
            session.to_string(),
            PersistedEvent::MessageAppended {
                message: Message::user_text(text),
            },
        )
    }

    #[tokio::test]
    async fn append_then_load_returns_events() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01TEST".to_string();

        st.append(&id, make_record(1, &id, "first")).await.unwrap();
        st.append(&id, make_record(2, &id, "second")).await.unwrap();

        let records = st.load(&id).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[1].seq, 2);
    }

    #[tokio::test]
    async fn load_after_returns_only_later_events() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01AFTER".to_string();

        for seq in 1..=5 {
            st.append(&id, make_record(seq, &id, &format!("msg{seq}")))
                .await
                .unwrap();
        }

        let records = st.load_after(&id, 3).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 4);
        assert_eq!(records[1].seq, 5);
    }

    #[tokio::test]
    async fn next_seq_returns_max_plus_one() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01NEXTSEQ".to_string();

        // 空会话 → 1
        assert_eq!(st.next_seq(&id).await.unwrap(), 1);

        st.append(&id, make_record(1, &id, "a")).await.unwrap();
        st.append(&id, make_record(2, &id, "b")).await.unwrap();

        // 已有 2 条 → 下一个 seq = 3
        assert_eq!(st.next_seq(&id).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn delete_removes_event_file() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01DEL".to_string();

        st.append(&id, make_record(1, &id, "x")).await.unwrap();
        st.delete(&id).await.unwrap();

        let records = st.load(&id).await.unwrap();
        assert!(records.is_empty(), "expected empty: records");
    }

    #[tokio::test]
    async fn load_nonexistent_session_returns_empty() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let records = st.load(&"01NOEXIST".to_string()).await.unwrap();
        assert!(records.is_empty(), "expected empty: records");
    }

    #[tokio::test]
    async fn corrupted_line_returns_error() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01CORRUPT".to_string();
        let path = st.session_path(&id);

        // 写入合法行 + 损坏行
        st.append(&id, make_record(1, &id, "ok")).await.unwrap();
        tokio::fs::write(&path, "not-json\n").await.unwrap();

        let result = st.load(&id).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, StorageError::Corrupted(_)));
    }

    #[test]
    fn load_events_sync_works_without_runtime() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01SYNC".to_string();

        // 同步路径：手动写文件后用 load_events_sync 读
        let path = st.session_path(&id);
        let r1 = make_record(1, &id, "a");
        let r2 = make_record(2, &id, "b");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&r1).unwrap(),
            serde_json::to_string(&r2).unwrap()
        );
        std::fs::write(path.as_std_path(), content).unwrap();

        let records = st.load_events_sync(&id).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[1].seq, 2);

        // next_seq_sync 应返回 3
        assert_eq!(st.next_seq_sync(&id).unwrap(), 3);
    }

    #[test]
    fn next_seq_sync_empty_session_returns_1() {
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01EMPTY".to_string();
        assert_eq!(st.next_seq_sync(&id).unwrap(), 1);
    }

    #[tokio::test]
    async fn read_tail_line_handles_long_line() {
        // ST-R6-1（2026-08-28 R6 审查）：单行 > 8 KiB（MessageAppended 持久化
        // 大工具结果可达 1MiB）——8 KiB 窗口从行中部开始时，回退寻行首而非
        // 报 Corrupted。此前 R5 ST-9 修复引入回归：尾部窗口截断使 append 单调
        // 性检查恒失败（事件流冻结）且 next_seq_sync 失败（不可 resume/replay）。
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01LONGLINE".to_string();

        // 20 KiB 事件行（远超 8 KiB 窗口）
        let long_text = "x".repeat(20_000);
        st.append(&id, make_record(1, &id, &long_text))
            .await
            .unwrap();

        // next_seq_sync 应能正确解析末行 seq=1 + 1 = 2
        assert_eq!(st.next_seq_sync(&id).unwrap(), 2);

        // 第二条事件（验证 append 单调性检查不因解析失败而冻结）
        st.append(&id, make_record(2, &id, "short")).await.unwrap();

        let records = st.load(&id).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].seq, 2);
    }

    #[tokio::test]
    async fn read_tail_line_handles_last_line_shorter_with_prior_long_line() {
        // 长行后跟短行（常见模式：大工具结果后跟小事件）
        let dir = tempdir().unwrap();
        let st = storage(&dir);
        let id = "01MIXED".to_string();

        st.append(&id, make_record(1, &id, &"x".repeat(20_000)))
            .await
            .unwrap();
        st.append(&id, make_record(2, &id, "short")).await.unwrap();

        assert_eq!(st.next_seq_sync(&id).unwrap(), 3);
    }
}
