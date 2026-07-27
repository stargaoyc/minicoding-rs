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
//! - L0 不可覆盖：分发 `Hook` 前先应用内置黑名单 `Deny`，`Hook` 的 `allow` 对黑名单 `Deny`
//!   无效（C-21）。
//!
//! 详见 `docs/modules.md` §5、`docs/hooks.md`。

#![deny(clippy::all, clippy::pedantic)]
