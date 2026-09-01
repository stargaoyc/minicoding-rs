//! # minicoding-tools
//!
//! 内置 `Tool` 实现，组合层。
//!
//! 实现内置 `Tool` 集合（`fs`/`shell`/`web`/`git`/`task`/`plan`）；作为"组合层"，
//! 依赖 core + policy（架构守卫白名单），沙箱/Journal/MCP 等领域能力由 Runtime 经
//! core trait **注入 `ToolContext`**——本 crate 不直连领域 crate（ARCH-7，2026-08-26
//! R3 头注修正：原文描述的直连装配方式已被 trait 注入取代）。
//!
//! ## 设计要点
//!
//! - **路径沙箱**：`util::resolve_path` 本地规范化校验（C-03）；委托
//!   `minicoding-policy::path_sandbox::resolve_under`，不重复实现；
//! - **shell.run**：执行前调 `ctx.sandbox_driver`（Runtime 注入的 `SandboxDriver`）
//!   应用 OS 沙箱（第二道防线，C-22）；
//! - **fs.write/edit/delete + Journal**：成功后调 `ctx.journal.record`（Runtime 注入，
//!   仅 `file-undo` feature 时接线，C-28）；
//! - **task.create/update/list**：增量模型，状态机 `Pending→InProgress→Completed`
//!   不可跳跃（C-31）；
//! - **plan.exit**：退出 Plan 模式 + 缓存预批准，`SideEffect::None` 可穿透 Plan 硬门（C-25）；
//! - **mcp 工具包装**：见 `minicoding-mcp`（`naming`/wrapper 在该 crate，非此处）。
//!
//! 当前阶段：已实现 fs 工具组——只读（`fs.read`/`fs.list`/`fs.glob`/`fs.grep`，见
//! T-M1-6）与写入（`fs.write`/`fs.edit`/`fs.multiedit`/`fs.delete`，见 T-M2-3）；以及
//! shell 工具组（`shell.run`，见 T-M2-4）；task 工具组（`task.*`，见 T-M3-8）；
//! plan 工具组（`plan.exit`，见 T-M5-6）；其余工具（web/git/mcp 包装）与领域 crate
//! 依赖见 M3+/M4+。
//!
//! 详见 `docs/modules.md` §11、`docs/design.md` §6、§16、§18。

mod fs;
mod git;
mod memory;
mod plan;
mod shell;
mod skills;
mod task;
mod ui;
mod util;
mod worktree;

#[cfg(feature = "web")]
mod web;

pub use fs::{
    FsDelete, FsEdit, FsGlob, FsGrep, FsList, FsMultiEdit, FsRead, FsWrite,
    register_readonly_tools, register_write_tools,
};
pub use git::{GitApply, GitDiff, register_git_tools};
pub use memory::{
    AutoMemoryWriter, InMemoryAutoMemory, MemoryCategory, MemoryWrite, MemoryWriteTarget,
    register_memory_tools,
};
pub use plan::{PlanExit, register_plan_tools};
pub use shell::{
    BackgroundShellStatus, BackgroundShellStore, InMemoryBackgroundShellStore, ShellBackground,
    ShellKill, ShellOutput, ShellRun, register_shell_tools,
};
pub use skills::{SkillList, SkillRead, register_skill_tools};
pub use task::{
    InMemoryTaskStore, TaskCreate, TaskList, TaskPatch, TaskSpawn, TaskStore, TaskUpdate,
    register_spawn_tool, register_task_tools,
};
pub use ui::{UiAsk, register_ui_tools};
pub use util::{ensure_dir, resolve_path, truncate_output};
pub use worktree::WorktreeSubagentRunner;

#[cfg(feature = "web")]
pub use web::{WebFetch, WebSearch, register_web_tools};
