//! # minicoding-policy
//!
//! 权限实现：实现 `core::policy::PermissionPolicy`/`PermissionPrompter` trait。
//!
//! 职责：决策引擎、内置黑名单（不可覆盖）、`ApprovalMode`/`Preset` 解析、决策持久化、
//! 各 `Prompter` 实现（`Interactive`/`NonInteractive`/`Tui`/`Callback`）、命令风险解释、
//! 应用层路径沙箱（`sandbox_path`，第一道防线）。
//!
//! 设计要点：
//! - 黑名单最高优先级（C-02），任何用户配置与 `Hook` 都无法覆盖；
//! - 决策（policy）与交互（prompter）分离，解决 broadcast 无法承载点对点回复；
//! - 对 `AGENTS.md`/`CLAUDE.md` 写操作注入 `Verdict::Ask` 且不可 `AllowAlways`（C-23）。
//!
//! 详见 `docs/modules.md` §3、`docs/design.md` §9、`docs/security.md`。

mod builtin;
mod mode;
mod path_sandbox;
mod prompter;
mod redact;
mod replay;
mod ssrf;
mod tui;

pub use builtin::BuiltinPolicy;
pub use mode::{ApprovalMode, Preset};
pub use path_sandbox::{PathSandboxError, is_under, resolve_under};
pub use prompter::{
    AutoApprovePrompter, CallbackPrompter, InteractivePrompter, NonInteractivePrompter,
};
pub use redact::redact;
pub use replay::ReplayPolicy;
pub use ssrf::{SsrfError, SsrfOptions, check_host, check_ip, check_url};
pub use tui::TuiPrompter;
