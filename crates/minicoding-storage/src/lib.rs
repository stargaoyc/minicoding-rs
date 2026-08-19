//! # minicoding-storage
//!
//! 存储与审计：实现 `core::storage::Storage`/`AuditSink`/`EventStore`/`SnapshotStore` trait，
//! 并提供事件重放算法（`replay_session_state`，M-05 从 core 下沉，避免 core 含领域算法）。
//!
//! 职责：
//! - `JSONL` 会话日志（追加写、崩溃安全）；
//! - 审计日志（`audit.log` `JSONL`，Unix 下 0600 权限）；
//! - Event Sourcing（事件持久化 + snapshot + 重放，见 `design.md` §25）。
//!
//! ## 设计要点
//!
//! - **崩溃安全**：每条消息/事件 `append` 后 `fsync`，崩溃时磁盘与内存一致；
//! - **审计完整性**：`audit.log` 文件权限 0600，追加写不可篡改历史（无 update/delete
//!   `API`），见 AGENTS.md §5.5、`rules.md` C-04；
//! - **惰性物化**：空会话不产生文件（首条消息/事件时才创建会话文件）；
//! - **Event Sourcing**：`JsonlEventStore` 持久化状态变更事件，`JsonlSnapshotStore`
//!   周期性 snapshot 加速 replay，`replay_session_state` 重建 `Session` 状态。
//!
//! 详见 `docs/modules.md` §9、`docs/data-model.md`、`docs/design.md` §25。

#![deny(clippy::all, clippy::pedantic)]

mod audit;
mod event_store;
mod export;
mod index;
mod jsonl;
mod lock;
mod replay;
mod snapshot_store;

pub use audit::FileAuditSink;
pub use event_store::JsonlEventStore;
pub use export::{ExportFormat, export_session_jsonl, export_session_md};
pub use index::{SessionIndex, SessionIndexEntry};
pub use jsonl::JsonlStorage;
pub use lock::SessionLock;
pub use replay::{ReplayError, ReplayedSession, replay_session_state, session_from_messages};
pub use snapshot_store::JsonlSnapshotStore;
