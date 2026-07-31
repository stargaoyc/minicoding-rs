//! # minicoding-tools
//!
//! 内置 `Tool` 实现，组合层。
//!
//! 实现内置 `Tool` 集合（`fs`/`shell`/`web`/`git`/`task`/`plan`/`mcp` 包装）；作为
//! "组合层"，可依赖多个领域 crate（context/policy/memory/hooks/journal/sandbox/mcp/
//! storage）以完成工具执行闭环。
//!
//! ## 设计要点
//!
//! - **路径沙箱**：`util::resolve_path` 本地规范化校验（C-03）；M1-T7 起委托
//!   `minicoding-policy::path_sandbox::resolve_under`，不重复实现；
//! - **shell.run**：执行前调 `SandboxDriver::apply`（来自 `minicoding-sandbox`）应用 `OS` 沙箱；
//! - **fs.write/edit/delete + `Journal`**：成功后调 `Journal::record`（来自
//!   `minicoding-journal`），仅 `file-undo=true` 时生效；
//! - **task.create/update/list**：增量模型，状态机 `Pending→InProgress→Completed`
//!   不可跳跃（C-31）；
//! - **`mcp::wrapper`**：把 `McpServerConfig` + 远程 schema 包装为 `Tool`，`side_effect`
//!   据 `readOnlyHint`/`destructiveHint` 映射（C-25）。
//!
//! 当前阶段：已实现 fs 工具组——只读（`fs.read`/`fs.list`/`fs.glob`/`fs.grep`，见
//! T-M1-6）与写入（`fs.write`/`fs.edit`/`fs.multiedit`/`fs.delete`，见 T-M2-3）；以及
//! shell 工具组（`shell.run`，见 T-M2-4）；其余工具（web/git/task/plan/mcp 包装）与
//! 领域 crate 依赖见 M3+。
//!
//! 详见 `docs/modules.md` §11、`docs/design.md` §6。

#![deny(clippy::all, clippy::pedantic)]

mod fs;
mod shell;
mod util;

pub use fs::{
    FsDelete, FsEdit, FsGlob, FsGrep, FsList, FsMultiEdit, FsRead, FsWrite,
    register_readonly_tools, register_write_tools,
};
pub use shell::{ShellRun, register_shell_tools};
pub use util::{ensure_dir, resolve_path, truncate_output};
