//! Journal 记录辅助（T-M4-9 集成）。
//!
//! `fs.write`/`fs.edit`/`fs.delete` 成功后调用 [`record_change`] 把改动记入
//! `Journal`，供 `/undo` 回滚（C-28）。仅当 `ToolContext::journal` 注入时生效；
//! 未注入（`file-undo` feature 关闭）时为 no-op，不影响工具主流程。
//!
//! 记录失败仅打 `warn` 日志，不阻塞工具返回（best effort：journal 是辅助能力，
//! 失败不应阻断主写入操作）。

use minicoding_core::journal::{ChangeEntry, FileChange, Journal};
use std::sync::Arc;
use time::OffsetDateTime;

/// 把单次文件改动记入 journal（若注入）。
///
/// `op_id` 用 ULID 生成（每次工具调用唯一），`prompt_snippet` 留空（fs 工具
/// 不接触用户消息原文；若需展示，可由 Runtime 在 turn 级别补充）。
pub async fn record_change(journal: Option<&Arc<dyn Journal>>, change: FileChange) {
    let Some(j) = journal else {
        return;
    };
    let entry = ChangeEntry {
        op_id: ulid::Ulid::new().to_string(),
        ts: OffsetDateTime::now_utc(),
        prompt_snippet: String::new(),
        files: vec![change],
    };
    if let Err(e) = j.record(entry).await {
        tracing::warn!(error = %e, "journal record failed (best effort)");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::journal::{ChangeEntry, DiffEntry, FileChange, Journal, UndoReport};
    use minicoding_core::model::JournalError;
    use minicoding_core::provider::BoxFuture;
    use std::sync::{Arc, Mutex};

    /// 测试用 Journal：记录所有 record 调用，可配置为返回错误。
    struct MockJournal {
        recorded: Mutex<Vec<ChangeEntry>>,
        fail_record: bool,
    }

    impl MockJournal {
        fn new() -> Self {
            Self {
                recorded: Mutex::new(Vec::new()),
                fail_record: false,
            }
        }

        fn failing() -> Self {
            Self {
                recorded: Mutex::new(Vec::new()),
                fail_record: true,
            }
        }

        fn recorded_count(&self) -> usize {
            self.recorded.lock().expect("lock not poisoned").len()
        }

        fn last_entry(&self) -> ChangeEntry {
            self.recorded
                .lock()
                .expect("lock not poisoned")
                .last()
                .cloned()
                .expect("at least one entry recorded")
        }
    }

    impl Journal for MockJournal {
        fn record(&self, entry: ChangeEntry) -> BoxFuture<'_, Result<(), JournalError>> {
            Box::pin(async move {
                if self.fail_record {
                    Err(JournalError::Conflict("mock failure".to_string()))
                } else {
                    self.recorded.lock().expect("lock not poisoned").push(entry);
                    Ok(())
                }
            })
        }

        fn undo(&self, _steps: usize) -> BoxFuture<'_, Result<UndoReport, JournalError>> {
            Box::pin(async { Ok(UndoReport::default()) })
        }

        fn diff(&self) -> BoxFuture<'_, Result<Vec<DiffEntry>, JournalError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn reset_to_initial(&self) -> BoxFuture<'_, Result<(), JournalError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn record_change_none_journal_is_noop() {
        let change = FileChange::Created {
            path: Utf8PathBuf::from("/tmp/test.txt"),
            content: b"hello".to_vec(),
        };
        // journal = None → 立即返回，不 panic
        record_change(None, change).await;
    }

    #[tokio::test]
    async fn record_change_some_journal_records_entry() {
        let mock = Arc::new(MockJournal::new());
        let journal: Arc<dyn Journal> = mock.clone();
        let change = FileChange::Created {
            path: Utf8PathBuf::from("/tmp/test.txt"),
            content: b"hello".to_vec(),
        };
        record_change(Some(&journal), change).await;
        assert_eq!(mock.recorded_count(), 1);
        let entry = mock.last_entry();
        assert_eq!(entry.files.len(), 1);
        // op_id 是 ULID（26 字符）
        assert_eq!(entry.op_id.len(), 26);
        // fs 工具不接触用户消息原文
        assert!(entry.prompt_snippet.is_empty());
    }

    #[tokio::test]
    async fn record_change_failure_is_best_effort_no_panic() {
        let mock = Arc::new(MockJournal::failing());
        let journal: Arc<dyn Journal> = mock.clone();
        let change = FileChange::Created {
            path: Utf8PathBuf::from("/tmp/test.txt"),
            content: b"hello".to_vec(),
        };
        // record 失败仅打 warn 日志，不 panic，不返回错误（best effort）
        record_change(Some(&journal), change).await;
        // 失败时不应记录
        assert_eq!(mock.recorded_count(), 0);
    }
}
