//! 跨进程会话文件锁（`fs2` 排他锁）。
//!
//! 设计意图（见 `docs/features.md` S-03、`rules.md` C-22）：
//! - **同会话互斥**：两个进程同时 `--resume` 同一 `session_id`，第二个 `acquire` 失败
//!   返回 `StorageError::Locked`，避免并发写同一 `.jsonl` 导致交错损坏；
//! - **RAII 释放**：`SessionLock` 持有底层 `File`，`Drop` 时自动释放锁（fs2 锁随 fd 关闭释放）；
//! - **锁文件路径**：`{sessions_dir}/{session_id}.lock`，与 `.jsonl` 同目录便于清理。

use camino::{Utf8Path, Utf8PathBuf};
use fs2::FileExt;

/// R9 STR-6：`acquire_blocking` 等待排他锁的默认超时（持锁进程卡住时防
/// 永久挂起）。10s 对正常 append 热路径（亚毫秒级持锁）足够宽裕。
const BLOCKING_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
use minicoding_core::storage::StorageError;
use std::fs::File;

/// 会话排他锁的 RAII 守卫。
///
/// 持有锁文件句柄，`Drop` 时自动释放排他锁并关闭文件。不可 `Clone`（语义上单点持有）。
#[derive(Debug)]
pub struct SessionLock {
    file: File,
    path: Utf8PathBuf,
}

impl SessionLock {
    /// 对 `{session_id}.lock` 加排他锁。
    ///
    /// 文件不存在时创建（`create(true)` + `truncate(true)`，锁文件内容无意义）。
    /// 若已被其他进程持有排他锁，`try_lock_exclusive` 立即失败返回
    /// `StorageError::Locked`（非阻塞）。
    ///
    /// # Errors
    /// - `StorageError::Locked`：锁被占用；
    /// - `StorageError::Io`：文件创建或加锁时其他 IO 错误。
    pub fn acquire(path: impl Into<Utf8PathBuf>) -> Result<Self, StorageError> {
        let path = path.into();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path.as_std_path())?;
        file.try_lock_exclusive().map_err(|e| {
            // fs2 在锁竞争时返回 WouldBlock 类错误，统一映射为 Locked
            StorageError::Locked(format!("{}: {e}", path.as_str()))
        })?;
        Ok(Self { file, path })
    }

    /// 对 `{session_id}.lock` 加排他锁（**阻塞式**）。
    ///
    /// 与 [`Self::acquire`] 的区别：锁被其他进程持有时**阻塞等待**直到获得
    /// 而非立即失败。用于 `append` 热路径（M-01，修 S1-2）——同会话并发追加
    /// 时后者等待前者完成，避免两次 `write_all` 之间交错把两条消息并成一行
    /// 不可解析的 JSON。`--resume` 的单点检测仍走 [`Self::acquire`]（非阻塞，
    /// 检测到占用即报 `StorageError::Locked`）。
    ///
    /// fs2 的 `lock_exclusive` 是同步阻塞 API，调用方应在
    /// `tokio::task::spawn_blocking` 中执行，避免阻塞 async reactor。
    ///
    /// **R9 STR-6 修复**：改用 `try_lock_exclusive` + 轮询（10ms 间隔）替代
    /// 裸 `lock_exclusive`——后者在持锁进程**卡住（非崩溃）**时另一进程永久
    /// 挂起（flock 是 advisory，NFS 上更不可靠）。超时（默认 10s）返回
    /// `StorageError::Locked`，避免跨进程死锁不可恢复。
    ///
    /// # Errors
    /// - `StorageError::Io`：文件创建或加锁时 IO 错误；
    /// - `StorageError::Locked`：超时仍未获得锁。
    pub fn acquire_blocking(path: impl Into<Utf8PathBuf>) -> Result<Self, StorageError> {
        Self::acquire_blocking_timeout(path, BLOCKING_LOCK_TIMEOUT)
    }

    /// `acquire_blocking` 带显式超时（测试注入用）。
    fn acquire_blocking_timeout(
        path: impl Into<Utf8PathBuf>,
        timeout: std::time::Duration,
    ) -> Result<Self, StorageError> {
        let path = path.into();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path.as_std_path())?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok(()) = file.try_lock_exclusive() {
                return Ok(Self { file, path });
            }
            if std::time::Instant::now() >= deadline {
                return Err(StorageError::Locked(format!(
                    "{}: 等待排他锁超时（持锁进程可能卡住）",
                    path.as_str()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// 显式释放锁（等价于 drop，便于语义明确处调用）。
    ///
    /// 实际解锁由 `Drop` 完成以避免 fd 泄漏——`fs2::unlock` 在 fd 关闭前调用即可，
    /// 解锁失败不可恢复（fd 已失效），故不返回 `Result`。
    pub fn release(self) {
        // 消费 self 触发 Drop::drop，后者调用 unlock 并关闭 file。
        drop(self);
    }

    /// 返回锁文件路径。
    #[must_use]
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // fs2: unlock 在 fd 关闭前调用更安全；忽略错误（进程退出/panicked 时尽力释放）。
        // incompatible_msrv 误报：unlock 来自 fs2::FileExt（非 std），MSRV 1.99 可用。
        #![allow(clippy::incompatible_msrv)]
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn lock_path(dir: &tempfile::TempDir, id: &str) -> Utf8PathBuf {
        dir.path().join(format!("{id}.lock")).try_into().unwrap()
    }

    #[test]
    fn acquire_then_release_allows_reacquire() {
        let dir = tempdir().unwrap();
        let p = lock_path(&dir, "01AA");
        let lock = SessionLock::acquire(&p).expect("first acquire");
        drop(lock);
        // 释放后可再次获取
        let _lock2 = SessionLock::acquire(&p).expect("reacquire after release");
    }

    #[test]
    fn second_acquire_on_held_lock_fails_with_locked() {
        let dir = tempdir().unwrap();
        let p = lock_path(&dir, "01BB");
        let _first = SessionLock::acquire(&p).expect("first acquire");
        // 同一进程内对同一文件再次 acquire 也应失败（fs2 排他锁语义）
        let second = SessionLock::acquire(&p);
        match second {
            Err(StorageError::Locked(_)) => {}
            other => panic!("expected StorageError::Locked, got {other:?}"),
        }
    }

    #[test]
    fn different_sessions_lock_independently() {
        let dir = tempdir().unwrap();
        let p1 = lock_path(&dir, "01CC");
        let p2 = lock_path(&dir, "01DD");
        let _l1 = SessionLock::acquire(&p1).unwrap();
        // 不同会话锁互不影响
        let _l2 = SessionLock::acquire(&p2).unwrap();
    }
}
