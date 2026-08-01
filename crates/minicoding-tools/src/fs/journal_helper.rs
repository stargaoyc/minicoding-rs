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
