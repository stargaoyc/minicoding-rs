//! Shell 工具（`shell.run`）。

mod run;

pub use run::ShellRun;

use minicoding_core::tool::ToolRegistry;
use std::sync::Arc;

/// 注册全部 shell 工具到 `registry`（`SideEffect::Command`，需经权限审批）。
pub fn register_shell_tools(registry: &mut ToolRegistry) {
    registry.register(Arc::new(ShellRun::new()));
}
