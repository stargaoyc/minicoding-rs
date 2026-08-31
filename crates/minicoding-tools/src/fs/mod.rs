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

/// 判定路径是否指向敏感文件（`.env*`/`credentials`/`*.pem`/`*.key` 等）。
///
/// R10-12：从 `read.rs` 提取共享（此前仅 `fs.read` 脱敏，`fs.grep` 直接输出
/// 完整内容可绕过密钥脱敏）。供 `fs.read`/`fs.grep`/`git.diff` 等输出进入
/// 模型上下文的工具统一调用。
#[must_use]
pub(crate) fn is_sensitive_path(path: &camino::Utf8Path) -> bool {
    // 常量前置，避免 `items_after_statements` 警告。
    const EXACT: &[&str] = &["credentials", "creds"];
    const SENSITIVE_EXT: &[&str] = &["pem", "key", "pfx", "p12"];
    const KEYWORDS: &[&str] = &["secret", "password", "token"];

    let Some(file_name) = path.file_name() else {
        return false;
    };
    let lower = file_name.to_lowercase();

    // .env 系列精确/前缀匹配
    if lower == ".env" || lower.starts_with(".env.") {
        return true;
    }

    // 精确匹配
    if EXACT.contains(&lower.as_str()) {
        return true;
    }

    // 扩展名匹配
    if let Some(ext) = path.extension()
        && SENSITIVE_EXT.contains(&ext)
    {
        return true;
    }

    // 关键词包含匹配
    KEYWORDS.iter().any(|kw| lower.contains(kw))
}

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
