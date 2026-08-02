//! Shell 工具组（`shell.run` + `shell.background`/`shell.output`/`shell.kill`）。
//!
//! - `shell.run`：同步执行，等待完成返回输出（T-M2-4）；
//! - `shell.background`：异步 spawn，立即返回 `shell_id`（T-M8-5）；
//! - `shell.output`：非阻塞读取已累积的 stdout/stderr（T-M8-5）；
//! - `shell.kill`：终止后台 shell（T-M8-5）。
//!
//! 后台 shell 由 [`BackgroundShellStore`] 抽象管理（与 `TaskStore` 同构），
//! 默认实现 [`InMemoryBackgroundShellStore`] 持有 `tokio::sync::Mutex<HashMap>`。

mod background;
mod kill;
mod output;
mod run;

pub use background::{
    BackgroundShellStatus, BackgroundShellStore, InMemoryBackgroundShellStore, ShellBackground,
};
pub use kill::ShellKill;
pub use output::ShellOutput;
pub use run::ShellRun;

use minicoding_core::tool::ToolRegistry;
use std::sync::Arc;

/// 注册全部 shell 工具到 `registry`（`SideEffect::Command`，需经权限审批）。
///
/// `shell.background`/`output`/`kill` 共享一个 [`InMemoryBackgroundShellStore`]。
/// Runtime 若需自定义存储，可用各工具的 `new(store)` 构造后自行注册。
pub fn register_shell_tools(registry: &mut ToolRegistry) {
    let store: Arc<dyn BackgroundShellStore> = Arc::new(InMemoryBackgroundShellStore::new());
    registry.register(Arc::new(ShellRun::new()));
    registry.register(Arc::new(ShellBackground::new(store.clone())));
    registry.register(Arc::new(ShellOutput::new(store.clone())));
    registry.register(Arc::new(ShellKill::new(store)));
}
