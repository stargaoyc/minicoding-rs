//! `SandboxDriver` trait + `SandboxPolicy` + `NoopDriver` 兜底（见 `api.md` §3.9）。
//!
//! M1 仅提供 `NoopDriver`，M4 实现真实 Landlock/Seatbelt 驱动。
//!
//! `SandboxDriver` 是同步 trait（`apply` 在子进程 exec 前同步调用），无需 `BoxFuture`。

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

/// OS 级沙箱策略（第二道防线）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxPolicy {
    ReadOnly,
    WorkspaceWrite {
        workdir: Utf8PathBuf,
        writable: Vec<Utf8PathBuf>,
    },
    ExternalSandbox,
    DangerFullAccess,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::WorkspaceWrite {
            workdir: Utf8PathBuf::from("."),
            writable: Vec::new(),
        }
    }
}

/// 沙箱驱动 trait（同步、`dyn` 兼容）。
///
/// `apply` 在子进程 `exec` 前同步调用，应用内核级限制。
pub trait SandboxDriver: Send + Sync {
    /// 在子进程 exec 前应用沙箱策略。
    ///
    /// # Errors
    /// 沙箱策略应用失败（如内核限制不可用、IO 失败）时返回 `SandboxError`。
    fn apply(
        &self,
        policy: &SandboxPolicy,
        cmd: &mut std::process::Command,
    ) -> Result<(), SandboxError>;

    /// 当前平台是否原生支持硬隔离。
    fn is_hardened(&self) -> bool;

    /// 平台名。
    fn id(&self) -> &'static str;
}

/// 沙箱错误。
#[derive(thiserror::Error, Debug)]
pub enum SandboxError {
    #[error("sandbox: {0}")]
    Sandbox(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// 无操作驱动（兜底，未启用 sandbox feature 时使用）。
pub struct NoopDriver;

impl SandboxDriver for NoopDriver {
    fn apply(
        &self,
        _policy: &SandboxPolicy,
        _cmd: &mut std::process::Command,
    ) -> Result<(), SandboxError> {
        Ok(())
    }

    fn is_hardened(&self) -> bool {
        false
    }

    fn id(&self) -> &'static str {
        "noop"
    }
}
