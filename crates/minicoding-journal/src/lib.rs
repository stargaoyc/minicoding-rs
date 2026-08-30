//! # minicoding-journal
//!
//! 文件改动事务与回滚：实现 `core::journal::Journal` trait（T-M4-9）。
//!
//! 职责：会话内文件改动账本、`/undo` operation 级回滚、`/diff` 列出变更、
//! `/new` 回到会话启动状态、冲突检测。
//!
//! ## 设计要点（C-28）
//!
//! - **不落盘**：journal 含文件原文，落盘等于多存一份敏感数据，故仅驻留内存、
//!   会话结束即销毁；
//! - **冲突检测不强行覆盖**：恢复前比对当前文件内容与 `after`，不一致记入
//!   `failed_files`（用户可能在外部编辑器改过）；
//! - **不可越界恢复**：恢复路径经规范化校验（拒绝 `..` 越界，C-03/C-28）；
//! - **特性门控**：`file-undo` feature 默认关闭，开启时由 `Runtime` 持有。
//!
//! 详见 `docs/modules.md` §6、`docs/design.md` §17。

mod journal_impl;

pub use journal_impl::FileChangeJournal;

/// re-export core 的 trait 与关联类型，便于调用方单点导入。
pub use minicoding_core::journal::{
    ChangeEntry, DiffEntry, FileChange, Journal, JournalError, OpId, UndoReport,
};
