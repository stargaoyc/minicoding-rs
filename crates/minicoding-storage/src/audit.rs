//! 审计日志 sink：`JSONL` 追加写，Unix 下 0600 权限（AGENTS.md §5.5、C-04）。

use camino::{Utf8Path, Utf8PathBuf};
use minicoding_core::otel::span_name;
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
    #[tracing::instrument(skip(self), fields(otel.name = span_name::AUDIT_RECORD))]
    fn record(&self, rec: AuditRecord) -> BoxFuture<'_, Result<(), StorageError>> {
        let path = self.path.clone();
        Box::pin(async move {
            let line =
                serde_json::to_string(&rec).map_err(|e| StorageError::Serialize(e.to_string()))?;
            // tokio::fs::OpenOptions 不支持 mode()，使用 spawn_blocking 调 std
            // 以在 Unix 下原子设置 0600（create 时生效；已存在的宽权限文件
            // 兜底收紧——与 jsonl tighten_existing 同语义，2026-08-25 审查 L1）
            let inner = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                use std::io::Write;
                let file = open_for_append(&path)?;
                // tighten_existing 仅 unix 定义（Windows 无 POSIX 权限位）
                #[cfg(unix)]
                tighten_existing(&file);
                let mut file = file;
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

/// 已存在文件的权限兜底收紧到 0600（历史宽权限文件，L1）。
#[cfg(unix)]
fn tighten_existing(file: &std::fs::File) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = file.metadata()
        && meta.permissions().mode() & 0o777 != 0o600
    {
        let mut perm = meta.permissions();
        perm.set_mode(0o600);
        let _ = file.set_permissions(perm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::storage::AuditKind;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    /// 构造一条样本审计记录，便于多个测试复用。
    fn sample_record(detail: &str) -> AuditRecord {
        AuditRecord {
            ts: OffsetDateTime::now_utc(),
            session: "01TESTSESSION".to_string(),
            kind: AuditKind::PermissionResolved,
            tool: Some("fs.write".to_string()),
            decision: Some("allow".to_string()),
            detail: detail.to_string(),
        }
    }

    /// 把临时目录下的文件名转为 `Utf8PathBuf`。
    fn utf8_path(dir: &tempfile::TempDir, name: &str) -> Utf8PathBuf {
        dir.path()
            .join(name)
            .try_into()
            .expect("tempdir path 应为 UTF-8")
    }

    #[tokio::test]
    async fn new_does_not_create_file() {
        let dir = tempdir().expect("创建临时目录");
        let path = utf8_path(&dir, "audit.log");
        // 仅构造实例不应触发文件创建（懒创建：首次 record 时才创建）
        let _sink = FileAuditSink::new(path.clone());
        assert!(!path.exists(), "构造 FileAuditSink 不应创建文件");
    }

    #[tokio::test]
    async fn record_creates_file_and_writes_jsonl() {
        let dir = tempdir().expect("创建临时目录");
        let path = utf8_path(&dir, "audit.log");
        let sink = FileAuditSink::new(path.clone());
        sink.record(sample_record("first"))
            .await
            .expect("写入审计记录");

        assert!(path.exists(), "首次 record 后应创建 audit.log");
        let content = std::fs::read_to_string(path.as_std_path()).expect("读取 audit.log");
        // JSONL：恰好一行，且以换行结尾
        assert_eq!(content.lines().count(), 1);
        assert!(content.ends_with('\n'));

        let parsed: AuditRecord = serde_json::from_str(content.trim()).expect("解析 JSONL 行");
        assert_eq!(parsed.detail, "first");
        assert_eq!(parsed.tool.as_deref(), Some("fs.write"));
        assert_eq!(parsed.decision.as_deref(), Some("allow"));
        assert_eq!(parsed.session, "01TESTSESSION");
        assert!(matches!(parsed.kind, AuditKind::PermissionResolved));
    }

    #[tokio::test]
    async fn record_appends_does_not_overwrite() {
        let dir = tempdir().expect("创建临时目录");
        let path = utf8_path(&dir, "audit.log");
        let sink = FileAuditSink::new(path.clone());

        // 连续写三条，验证追加写不覆盖
        sink.record(sample_record("first"))
            .await
            .expect("写第 1 条");
        sink.record(sample_record("second"))
            .await
            .expect("写第 2 条");
        sink.record(sample_record("third"))
            .await
            .expect("写第 3 条");

        let content = std::fs::read_to_string(path.as_std_path()).expect("读取 audit.log");
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3, "应追加为 3 行而非覆盖");

        let r1: AuditRecord = serde_json::from_str(lines[0]).expect("解析第 1 行");
        let r2: AuditRecord = serde_json::from_str(lines[1]).expect("解析第 2 行");
        let r3: AuditRecord = serde_json::from_str(lines[2]).expect("解析第 3 行");
        assert_eq!(r1.detail, "first");
        assert_eq!(r2.detail, "second");
        assert_eq!(r3.detail, "third");
    }

    #[tokio::test]
    async fn record_persists_after_sync_all() {
        // record 返回前已 sync_all，重新读取应与首次读取一致
        let dir = tempdir().expect("创建临时目录");
        let path = utf8_path(&dir, "audit.log");
        let sink = FileAuditSink::new(path.clone());
        sink.record(sample_record("persisted"))
            .await
            .expect("写入审计记录");

        let content1 = std::fs::read_to_string(path.as_std_path()).expect("首次读取");
        let content2 = std::fs::read_to_string(path.as_std_path()).expect("二次读取");
        assert_eq!(content1, content2, "fsync 后内容应稳定持久化");
        assert!(content1.contains("persisted"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn record_creates_file_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().expect("创建临时目录");
        let path = utf8_path(&dir, "audit.log");
        let sink = FileAuditSink::new(path.clone());
        sink.record(sample_record("perm"))
            .await
            .expect("写入审计记录");

        let meta = std::fs::metadata(path.as_std_path()).expect("读取文件元数据");
        let mode = meta.permissions().mode();
        // 仅属主可读写（C-04、AGENTS.md §5.5）
        assert_eq!(
            mode & 0o777,
            0o600,
            "audit.log 权限应为 0600，实际为 {mode:o}"
        );
    }

    #[tokio::test]
    async fn record_preserves_all_record_fields() {
        let dir = tempdir().expect("创建临时目录");
        let path = utf8_path(&dir, "audit.log");
        let sink = FileAuditSink::new(path.clone());

        let ts = OffsetDateTime::now_utc();
        let rec = AuditRecord {
            ts,
            session: "01FIELDS".to_string(),
            kind: AuditKind::ToolCall,
            tool: Some("shell.run".to_string()),
            decision: Some("deny".to_string()),
            detail: "blocked by blacklist".to_string(),
        };
        sink.record(rec).await.expect("写入审计记录");

        let content = std::fs::read_to_string(path.as_std_path()).expect("读取 audit.log");
        let parsed: AuditRecord = serde_json::from_str(content.trim()).expect("解析 JSONL 行");

        // 全字段往返一致
        assert_eq!(parsed.session, "01FIELDS");
        assert_eq!(parsed.tool.as_deref(), Some("shell.run"));
        assert_eq!(parsed.decision.as_deref(), Some("deny"));
        assert_eq!(parsed.detail, "blocked by blacklist");
        assert!(matches!(parsed.kind, AuditKind::ToolCall));
        // OffsetDateTime 默认以 RFC3339 序列化，应可无损往返
        assert_eq!(parsed.ts, ts);
    }
}
