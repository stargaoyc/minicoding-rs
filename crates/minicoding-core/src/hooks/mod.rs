//! # Hooks 抽象层
//!
//! `Hook` trait + `HookRegistry` trait + 10 类生命周期事件 + 输入输出 DTO。
//!
//! 定义在 core（抽象层），实现在 `minicoding-hooks`（领域 crate）。Runtime 持有
//! `Arc<dyn HookRegistry>` 不需知道具体实现。
//!
//! ## 10 类事件
//!
//! `SessionStart`/`UserPromptSubmit`/`PreToolUse`/`PostToolUse`/`PostToolUseFailure`/
//! `PreCompact`/`PostCompact`/`Stop`/`SubagentStop`/`PermissionRequest`（见 `hooks.md` §2）。
//!
//! ## 与权限的关系
//!
//! `PreToolUse` Hook 在 `PermissionPolicy::check` **之后**运行，可把 `Ask` 升级为
//! `Allow`/`Deny`，但**不可**把内置黑名单的 `Deny` 改为 `Allow`（L0 不可覆盖，C-21）。
//!
//! 详见 `docs/hooks.md`、`docs/design.md` §20。

pub mod trait_def;

pub use trait_def::{
    AsyncRewakeScheduler, AsyncRewakeSpec, DispatchConfig, DispatchResult, Hook, HookDecision,
    HookError, HookEvent, HookInput, HookMatcher, HookOutput, HookRegistry,
    NoopAsyncRewakeScheduler, NoopHookRegistry, OnHookError, RewakeOutcome, VerdictSerde,
};
