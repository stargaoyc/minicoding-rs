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
/// `post_spawn` 在 `spawn()` 后调用，供需要 post-spawn 设置的平台（如 Windows
/// Job Object）使用；默认 no-op，Linux/macOS 不需覆写。
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

    /// 在 `spawn()` 后调用，供需要 post-spawn 设置的平台使用。
    ///
    /// Windows Job Object 驱动在此创建 Job Object、分配子进程、恢复线程
    /// （`apply` 仅设置 `CREATE_SUSPENDED` 标志）。Linux/macOS 不需覆写
    /// （沙箱在 `pre_exec` 内一次性应用完成）。
    ///
    /// # Errors
    /// post-spawn 设置失败（如 Job Object 创建/分配失败）时返回 `SandboxError`。
    fn post_spawn(&self, _pid: u32) -> Result<(), SandboxError> {
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    //! `SandboxPolicy` 默认值、`NoopDriver` 与 `SandboxError` 测试（覆盖率补全）。

    use super::*;

    #[test]
    fn sandbox_policy_default_is_workspace_write_dot() {
        let p = SandboxPolicy::default();
        match p {
            SandboxPolicy::WorkspaceWrite { workdir, writable } => {
                assert_eq!(workdir, camino::Utf8PathBuf::from("."));
                assert!(writable.is_empty(), "expected empty: writable");
            }
            other => panic!("expected WorkspaceWrite, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_policy_serde_roundtrip() {
        let p = SandboxPolicy::ReadOnly;
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"kind\":\"read_only\""));
        let decoded: SandboxPolicy = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(decoded, SandboxPolicy::ReadOnly));

        let p = SandboxPolicy::WorkspaceWrite {
            workdir: camino::Utf8PathBuf::from("/tmp/proj"),
            writable: vec![camino::Utf8PathBuf::from("/tmp")],
        };
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"kind\":\"workspace_write\""));
        let decoded: SandboxPolicy = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            SandboxPolicy::WorkspaceWrite { workdir, writable } => {
                assert_eq!(workdir, camino::Utf8PathBuf::from("/tmp/proj"));
                assert_eq!(writable.len(), 1);
            }
            other => panic!("expected WorkspaceWrite, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_policy_serde_external_sandbox_and_danger() {
        let p = SandboxPolicy::ExternalSandbox;
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"kind\":\"external_sandbox\""));

        let p = SandboxPolicy::DangerFullAccess;
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"kind\":\"danger_full_access\""));
    }

    #[test]
    fn noop_driver_apply_returns_ok() {
        let driver = NoopDriver;
        let mut cmd = std::process::Command::new("echo");
        let policy = SandboxPolicy::default();
        let result = driver.apply(&policy, &mut cmd);
        assert!(result.is_ok());
    }

    #[test]
    fn noop_driver_is_not_hardened_and_has_id_noop() {
        let driver = NoopDriver;
        assert!(!driver.is_hardened());
        assert_eq!(driver.id(), "noop");
    }

    #[test]
    fn noop_driver_post_spawn_default_is_ok() {
        let driver = NoopDriver;
        // 默认实现的 `post_spawn` 应返回 Ok
        let result = driver.post_spawn(12345);
        assert!(result.is_ok());
    }

    #[test]
    fn sandbox_error_display_sandbox() {
        let e = SandboxError::Sandbox("kernel not supported".to_string());
        assert_eq!(e.to_string(), "sandbox: kernel not supported");
    }

    #[test]
    fn sandbox_error_display_io() {
        let e = SandboxError::Io(std::io::Error::other("disk full"));
        let s = e.to_string();
        assert!(s.starts_with("io:"));
        assert!(s.contains("disk full"));
    }

    #[test]
    fn sandbox_error_from_io() {
        let io_err = std::io::Error::other("test");
        let sandbox_err: SandboxError = io_err.into();
        assert!(matches!(sandbox_err, SandboxError::Io(_)));
    }
}
