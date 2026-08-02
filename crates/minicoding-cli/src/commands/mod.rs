//! CLI 子命令（`session`/`exec`/`doctor`/`mcp`/`cred`/`serve`/`backup`，
//! T-M3-10c/T-M4-10/T-M4-11/T-M8-2/S-05）。
//!
//! - `session list`/`delete`：会话管理（不构建 Runtime）；
//! - `exec`：非交互批量执行（构建 Runtime + 沙箱策略）；
//! - `doctor --security`：安全自检（沙箱驱动/硬化状态）；
//! - `mcp list`/`approve`/`reject`/`reset-project-choices`：MCP server 管理（`mcp` feature）；
//! - `cred store`/`load`/`delete`：API key 凭证管理（keyring + 文件 fallback）；
//! - `serve`：启动 HTTP/SSE server（`serve` feature，T-M8-2）；
//! - `backup create`/`list`：打包 `~/.minicoding/` 为 tar.gz（S-05）。
//!
//! `session`/`doctor`/`mcp`/`cred`/`backup` 不构建 Runtime（无需 API key），直接复用存储层或探测函数。
//! `exec` 构建完整 Runtime 但强制非交互。`serve` 委托 `minicoding_server::serve`。

pub mod backup;
pub mod cred;
pub mod doctor;
pub mod exec;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "serve")]
pub mod serve;
pub mod session_cmd;

pub use backup::{BackupCommand, run_backup_command};
pub use cred::{CredCommand, run_cred_command};
pub use doctor::DoctorCommand;
pub use exec::ExecCommand;
#[cfg(feature = "mcp")]
pub use mcp::McpCommand;
#[cfg(feature = "serve")]
pub use serve::{ServeCommand, run_serve_command};
pub use session_cmd::{SessionCommand, run_session_command};
