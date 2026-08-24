//! # minicoding-hooks
//!
//! Hooks 实现：实现 `core::hooks::Hook`/`HookRegistry` trait。
//!
//! 职责：`Hook` 注册与串行聚合、`ScriptHook` 适配器（外部可执行 + `JSON` over stdio）、
//! `asyncRewake` 异步唤醒管理、6 个内置示例 `Hook`。
//!
//! 设计要点：
//! - 10 类生命周期事件（`SessionStart`/`UserPromptSubmit`/`PreToolUse`/`PostToolUse`/
//!   `PostToolUseFailure`/`PreCompact`/`PostCompact`/`Stop`/`SubagentStop`/
//!   `PermissionRequest`，见 `hooks.md` §2）；
//! - `asyncRewake` 后台进程同等待遇：遵守凭证隔离（C-04）、沙箱（C-22）、路径沙箱
//!   （C-03）约束（C-26）；
//! - L0 不可覆盖：`dispatch` 默认实现内置 L0 优先（C-21），Hook 的 `allow` 对
//!   黑名单 `Deny` 无效。
//!
//! `dispatch` 串行聚合逻辑由 core trait 默认实现提供（编排，非领域实现）；
//! 本 crate 提供 `HookRegistryImpl`（存储）与 `ScriptHook`（子进程协议，T-M5-2）。
//!
//! 详见 `docs/modules.md` §5、`docs/hooks.md`。

#![deny(clippy::all, clippy::pedantic)]

mod async_rewake;
mod builtin;
mod dispatch;
mod protocol;
mod registry;
mod script;

pub use async_rewake::{
    AsyncRewakeManager, DEFAULT_MAX_CONCURRENT, ManagedRewakeScheduler, RewakeResult, RewakeStatus,
};
pub use builtin::{
    AutoApproveTests, BackupBeforeCompact, BlockSecrets, FmtOnWrite, GitStatusInject, TestOnStop,
    builtin_hooks,
};
pub use protocol::{decode_output, encode_input, map_exit_code};
pub use registry::HookRegistryImpl;
pub use script::ScriptHook;
