//! # minicoding-memory
//!
//! 记忆实现：实现 `core::memory::ProjectDocLoader`/`MemoryStore` trait。
//!
//! 职责：长期记忆双文件（`long_term.md` + `index.json`）、Auto memory 自动学习、
//! 会话摘要、`AGENTS.md` 分层加载、记忆注入 system 段。
//!
//! 设计要点：
//! - Auto memory 物理隔离：`auto.md` 与 `long_term.md` 分离存储，对 `long_term.md`
//!   写入走 `Ask`，对 `auto.md` 隐式写入 `Allow`（C-27）；
//! - 指令性内容检测：`auto.md` 中含 `AGENTS.md` 风格指令性内容时降级 `Ask`（防绕过 C-23）；
//! - mtime 缓存：用 mtime 判断文件变更，无变更零 `IO`/分词（M-04）。
//!
//! 详见 `docs/modules.md` §4、`docs/design.md` §8。

#![deny(clippy::all, clippy::pedantic)]

pub mod auto;
pub mod inject;
pub mod long_term;
pub mod project_doc;
pub mod session_sum;

pub use auto::{AutoCategory, AutoMemory, is_instructional};
pub use inject::{inject_auto_memory, inject_memory};
pub use long_term::LongTermMemory;
pub use minicoding_core::memory::{MemoryStore, ProjectDocLoader};
pub use project_doc::{
    ProjectDocLoaderImpl, find_repo_root, inject_project_doc, inject_project_doc_sync,
};
pub use session_sum::SessionSummarizerImpl;
