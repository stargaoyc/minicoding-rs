//! # minicoding-storage
//!
//! 存储与审计：实现 `core::storage::Storage`/`AuditSink` trait。
//!
//! 职责：`JSONL` 会话日志（追加写、崩溃安全）、审计日志（`audit.log` `JSONL`，
//! Unix 下 0600 权限）。
//!
//! ## 设计要点
//!
//! - **崩溃安全**：每条消息 `append` 后 `fsync`，崩溃时磁盘与内存一致；
//! - **审计完整性**：`audit.log` 文件权限 0600，追加写不可篡改历史（无 update/delete
//!   `API`），见 AGENTS.md §5.5、`rules.md` C-04；
//! - **惰性物化**：空会话不产生文件（首条消息时才创建会话文件）。
//!
//! 详见 `docs/modules.md` §9、`docs/data-model.md`。

#![deny(clippy::all, clippy::pedantic)]

mod audit;
mod jsonl;

pub use audit::FileAuditSink;
pub use jsonl::JsonlStorage;
