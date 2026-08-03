//! Git 工具组（`git.diff`/`git.apply`，T-M8-5）。
//!
//! 通过 `git` CLI 实现（不自研 libgit2 绑定，AGENTS.md §3.6）；与 `shell.run`
//! 共享 `tokio::process::Command`，但提供结构化输入/输出与路径沙箱（C-03）。
//!
//! - `git.diff`：只读，返回 worktree diff（`SideEffect::None`）；
//! - `git.apply`：应用 patch 到 worktree（`SideEffect::FileWrite`，经权限审批）。

mod apply;
mod diff;

pub use apply::GitApply;
pub use diff::GitDiff;

use minicoding_core::tool::ToolRegistry;

/// 注册全部 git 工具到 `registry`。
pub fn register_git_tools(registry: &mut ToolRegistry) {
    registry.register(std::sync::Arc::new(GitDiff::new()));
    registry.register(std::sync::Arc::new(GitApply::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_git_tools_registers_both_tools() {
        let mut registry = ToolRegistry::new();
        register_git_tools(&mut registry);
        assert!(registry.get("git.diff").is_some());
        assert!(registry.get("git.apply").is_some());
        assert_eq!(registry.len(), 2);
    }
}
