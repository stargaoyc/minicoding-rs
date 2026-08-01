//! 文件系统工具（只读：`fs.read`/`fs.list`/`fs.glob`/`fs.grep`；写入：`fs.write`/
//! `fs.edit`/`fs.multiedit`/`fs.delete`）。

mod delete;
mod edit;
mod glob;
mod grep;
mod journal_helper;
mod list;
mod multiedit;
mod read;
mod write;

pub use delete::FsDelete;
pub use edit::FsEdit;
pub use glob::FsGlob;
pub use grep::FsGrep;
pub use list::FsList;
pub use multiedit::FsMultiEdit;
pub use read::FsRead;
pub use write::FsWrite;

use minicoding_core::tool::ToolRegistry;
use std::sync::Arc;

/// 注册全部只读 fs 工具到 `registry`。
pub fn register_readonly_tools(registry: &mut ToolRegistry) {
    registry.register(Arc::new(FsRead::new()));
    registry.register(Arc::new(FsList::new()));
    registry.register(Arc::new(FsGlob::new()));
    registry.register(Arc::new(FsGrep::new()));
}

/// 注册全部写入 fs 工具到 `registry`（`SideEffect::FileWrite`，需经权限审批）。
pub fn register_write_tools(registry: &mut ToolRegistry) {
    registry.register(Arc::new(FsWrite::new()));
    registry.register(Arc::new(FsEdit::new()));
    registry.register(Arc::new(FsMultiEdit::new()));
    registry.register(Arc::new(FsDelete::new()));
}
