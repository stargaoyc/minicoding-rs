//! # minicoding-journal
//!
//! 文件改动事务与回滚：实现 `core::journal::Journal` trait。
//!
//! 职责：会话内文件改动账本、`/undo` operation 级回滚、`/new` 会话级重置、冲突检测。
//!
//! 设计要点：
//! - **不落盘**：journal 含文件原文，落盘等于多存一份敏感数据，故仅驻留内存、会话结束
//!   即销毁（C-28）；
//! - **冲突检测不强行覆盖**：恢复前比对当前文件内容与 `after`，不一致记入 `failed_files`
//!   （C-28）；
//! - **特性门控**：`file-undo` feature 默认关闭，开启时由 `Runtime` 持有。
//!
//! 详见 `docs/modules.md` §6、`docs/design.md` §17。

#![deny(clippy::all, clippy::pedantic)]
