//! 项目文档分层加载（AGENTS.md / fallback），实现 `core::memory::ProjectDocLoader`。
//!
//! 模块结构：
//! - [`fallback`]：查找首个 fallback 文件、向上探测仓库根；
//! - [`loader`]：`ProjectDocLoaderImpl` 分层加载 + 截断 + skip；
//! - [`inject`]：包裹 `<project_doc>` 边界注入 system 段。
//!
//! 详见 `design.md` §8.6。

pub mod fallback;
pub mod inject;
pub mod loader;

pub use fallback::{find_project_doc, find_repo_root};
pub use inject::{PROJECT_DOC_BOUNDARY, inject_project_doc, inject_project_doc_sync};
pub use loader::ProjectDocLoaderImpl;
