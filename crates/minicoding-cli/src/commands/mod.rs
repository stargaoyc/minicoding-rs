//! CLI 子命令（`session list`/`delete` 等，T-M3-10c）。
//!
//! 子命令不构建完整 `Runtime`（无需 API key），直接复用存储层同步方法，
//! 启动开销小，适合 `session list` 万级会话 < 1s 的验收门槛（见 `dev-plan.md` §T-M3-10）。

pub mod session_cmd;

pub use session_cmd::{SessionCommand, run_session_command};
