//! 只读文件系统工具（`fs.read`/`fs.list`/`fs.glob`/`fs.grep`）。

mod glob;
mod grep;
mod list;
mod read;

pub use glob::FsGlob;
pub use grep::FsGrep;
pub use list::FsList;
pub use read::FsRead;

use minicoding_core::tool::ToolRegistry;
use std::sync::Arc;

/// 注册全部只读 fs 工具到 `registry`。
pub fn register_readonly_tools(registry: &mut ToolRegistry) {
    registry.register(Arc::new(FsRead::new()));
    registry.register(Arc::new(FsList::new()));
    registry.register(Arc::new(FsGlob::new()));
    registry.register(Arc::new(FsGrep::new()));
}
