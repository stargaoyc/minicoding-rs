//! 审计日志 sink：`JSONL` 追加写，Unix 下 0600 权限（AGENTS.md §5.5、C-04）。

use camino::{Utf8Path, Utf8PathBuf};
use minicoding_core::provider::BoxFuture;
use minicoding_core::storage::{AuditRecord, AuditSink, StorageError};

/// 文件审计 sink：将 `AuditRecord` 以 `JSONL` 追加写入 `audit.log`。
///
/// Unix 下文件权限 0600（见 AGENTS.md §5.5、`rules.md` C-04）。每次写入后 `fsync`
/// 保证审计完整性。
pub struct FileAuditSink {
    path: Utf8PathBuf,
}

impl FileAuditSink {
    /// 创建审计 sink。文件在首次写入时创建（Unix 下 0600 权限）。
    #[must_use]
    pub fn new(path: Utf8PathBuf) -> Self {
        Self { path }
    }
}

impl AuditSink for FileAuditSink {
    fn record(&self, rec: AuditRecord) -> BoxFuture<'_, Result<(), StorageError>> {
        let path = self.path.clone();
        Box::pin(async move {
            let line =
                serde_json::to_string(&rec).map_err(|e| StorageError::Serialize(e.to_string()))?;
            // tokio::fs::OpenOptions 不支持 mode()，使用 spawn_blocking 调 std
            // 以在 Unix 下原子设置 0600（仅在 create 时生效）
            let inner = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                use std::io::Write;
                let mut file = open_for_append(&path)?;
                writeln!(file, "{line}")?;
                file.sync_all()?;
                Ok(())
            })
            .await
            .map_err(|e| StorageError::Io(std::io::Error::other(e)))?;
            inner?;
            Ok(())
        })
    }
}

/// 以追加模式打开文件，Unix 下创建时设置 0600 权限。
#[cfg(unix)]
fn open_for_append(path: &Utf8Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path.as_std_path())
}

/// 以追加模式打开文件（非 Unix 平台）。
#[cfg(not(unix))]
fn open_for_append(path: &Utf8Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path.as_std_path())
}
