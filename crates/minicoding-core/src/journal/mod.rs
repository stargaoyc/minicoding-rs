//! 文件改动事务 trait（见 `api.md` §3.11、`design.md` §17）。
//!
//! 实现在 `minicoding-journal`（`FileChangeJournal`，纯内存，不落盘）。
//!
//! `/undo` 是特性门控（`file_undo`，默认关），仅会话内有效，会话结束销毁（C-28：
//! 不落盘避免敏感数据多份存储；冲突检测不强行覆盖；不绕过权限回滚）。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容。

mod trait_def;

pub use trait_def::{ChangeEntry, DiffEntry, FileChange, Journal, OpId, UndoReport};

/// journal 错误已在 `model::error` 定义，此处复用（与 `storage::StorageError` 同模式）。
pub type JournalError = crate::model::JournalError;
