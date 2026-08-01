//! `ExternalSandbox` 驱动（T-M4-2）。
//!
//! 用于 CI/容器场景：当 minicoding 已运行在 Docker/Firecracker/CI runner 内时，
//! 外层容器提供隔离，再叠加本进程的 Landlock/Seatbelt 既冗余又易因容器权限不足
//! 而失败。此模式下 `is_hardened()` 返回 `false`，`apply` 为 no-op，仅应用层
//! 权限生效。启动时打 `info` 日志声明"依赖外部隔离"（C-22）。
//!
//! macOS/Windows 在 M4 降级为 `NoopDriver`（平台优先级 M5+/M6+），与此区分：
//! `ExternalSandbox` 是用户**显式选择**依赖外部隔离，`NoopDriver` 是**降级兜底**。

use minicoding_core::sandbox::{SandboxDriver, SandboxError, SandboxPolicy};

/// 外部沙箱驱动（依赖容器/CI 外层隔离，本进程不应用内核限制）。
///
/// 与 `NoopDriver` 区别：`ExternalSandboxDriver` 明确声明依赖外部隔离（用户主动
/// 选择 `ExternalSandbox` 策略），`apply` 对所有策略均 no-op；`NoopDriver` 是
/// 降级兜底（内核不支持硬隔离时）。两者 `is_hardened()` 都返回 `false`。
pub struct ExternalSandboxDriver;

impl ExternalSandboxDriver {
    /// 创建外部沙箱驱动。
    #[must_use]
    pub fn new() -> Self {
        tracing::info!(
            driver = "external-sandbox",
            "沙箱驱动声明依赖外部隔离（CI/容器场景，C-22），本进程不应用内核限制"
        );
        Self
    }
}

impl Default for ExternalSandboxDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxDriver for ExternalSandboxDriver {
    fn apply(
        &self,
        _policy: &SandboxPolicy,
        _cmd: &mut std::process::Command,
    ) -> Result<(), SandboxError> {
        // 依赖外部容器隔离，本进程不应用内核限制
        Ok(())
    }

    fn is_hardened(&self) -> bool {
        false
    }

    fn id(&self) -> &'static str {
        "external-sandbox"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn external_sandbox_is_not_hardened() {
        let d = ExternalSandboxDriver::new();
        assert!(!d.is_hardened());
        assert_eq!(d.id(), "external-sandbox");
    }

    #[test]
    fn apply_is_noop_for_all_policies() {
        let d = ExternalSandboxDriver::new();
        let mut cmd = std::process::Command::new("true");
        d.apply(&SandboxPolicy::ReadOnly, &mut cmd).unwrap();
        d.apply(
            &SandboxPolicy::WorkspaceWrite {
                workdir: ".".into(),
                writable: vec![],
            },
            &mut cmd,
        )
        .unwrap();
        d.apply(&SandboxPolicy::ExternalSandbox, &mut cmd).unwrap();
        d.apply(&SandboxPolicy::DangerFullAccess, &mut cmd).unwrap();
    }
}
