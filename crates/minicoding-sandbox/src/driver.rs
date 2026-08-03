//! 沙箱驱动探测与调度（T-M4-1/2）。
//!
//! `detect_driver()` 编译期按 `cfg!(target_os)` 选实现，运行期探测内核支持：
//! - Linux：探测 Landlock 可用性，可用返回 `LandlockDriver`，否则降级 `NoopDriver` + warn；
//! - macOS：Seatbelt（10.5+ 全版本支持），返回 `SeatbeltDriver`；
//! - Windows：Job Object（Vista+），返回 `WindowsJobDriver`。

use minicoding_core::sandbox::SandboxDriver;
// NoopDriver 仅在非 Windows 平台的降级路径使用（Windows 直接返回 WindowsJobDriver）
#[cfg(not(target_os = "windows"))]
use minicoding_core::sandbox::NoopDriver;

/// 探测到的驱动类型（供 `doctor --security` 报告，见 T-M4-10）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    /// Linux Landlock LSM（内核 5.13+，硬隔离）。
    Landlock,
    /// macOS Seatbelt（10.5+，文件系统隔离）。
    Seatbelt,
    /// Windows Job Object（Vista+，进程遏制 + UI 隔离）。
    WindowsToken,
    /// 无操作驱动（兜底，无硬隔离）。
    Noop,
}

impl DriverKind {
    /// 当前平台最佳驱动的字符串名（与 `SandboxDriver::id` 一致）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Landlock => "landlock",
            Self::Seatbelt => "seatbelt",
            Self::WindowsToken => "windows-token",
            Self::Noop => "noop",
        }
    }
}

/// 探测当前平台可用的最佳沙箱驱动。
///
/// 返回 `Box<dyn SandboxDriver>` 供 `RuntimeBuilder` 注入。
///
/// - Linux：内核支持 Landlock 时返回 `LandlockDriver`，否则降级 `NoopDriver` + warn；
/// - macOS：返回 `SeatbeltDriver`（10.5+ 全版本支持）；
/// - Windows：返回 `WindowsJobDriver`（Vista+ Job Object）。
///
/// 降级时 `is_hardened()` 如实返回 `false`（C-22）。
#[must_use]
#[allow(clippy::needless_return)] // cfg 门控跨平台分支需显式 return
pub fn detect_driver() -> Box<dyn SandboxDriver> {
    #[cfg(target_os = "linux")]
    {
        if crate::linux::landlock_available() {
            tracing::info!(driver = "landlock", "沙箱驱动已就绪（Landlock 硬隔离）");
            return Box::new(crate::linux::LandlockDriver::new());
        }
        tracing::warn!(
            driver = "noop",
            reason = "内核不支持 Landlock（需 Linux 5.13+），降级 NoopDriver"
        );
        return Box::new(NoopDriver);
    }

    #[cfg(target_os = "macos")]
    {
        if crate::macos::seatbelt_available() {
            tracing::info!(
                driver = "seatbelt",
                "沙箱驱动已就绪（Seatbelt 文件系统隔离）"
            );
            return Box::new(crate::macos::SeatbeltDriver::new());
        }
        tracing::warn!(
            driver = "noop",
            reason = "Seatbelt 不可用（需 macOS 10.5+），降级 NoopDriver"
        );
        return Box::new(NoopDriver);
    }

    #[cfg(target_os = "windows")]
    {
        tracing::info!(
            driver = "windows-token",
            "沙箱驱动已就绪（Job Object 进程遏制）"
        );
        return Box::new(crate::windows::WindowsJobDriver::new());
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        tracing::warn!(driver = "noop", reason = "未支持的平台，降级 NoopDriver");
        Box::new(NoopDriver)
    }
}

/// 探测当前平台驱动类型（不构造实例，供 `doctor --security` 报告）。
///
/// 与 `detect_driver()` 的探测逻辑一致，但返回枚举而非实例，便于诊断输出。
#[must_use]
#[allow(clippy::needless_return)] // cfg 门控跨平台分支需显式 return
pub fn detect_driver_kind() -> DriverKind {
    #[cfg(target_os = "linux")]
    {
        if crate::linux::landlock_available() {
            return DriverKind::Landlock;
        }
        return DriverKind::Noop;
    }

    #[cfg(target_os = "macos")]
    {
        if crate::macos::seatbelt_available() {
            return DriverKind::Seatbelt;
        }
        return DriverKind::Noop;
    }

    #[cfg(target_os = "windows")]
    {
        return DriverKind::WindowsToken;
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        DriverKind::Noop
    }
}

#[cfg(test)]
mod tests {
    //! `DriverKind` 字符串映射与 `detect_driver*` 一致性测试（覆盖率补全）。
    //!
    //! `detect_driver` / `detect_driver_kind` 的平台分支由 `cfg(target_os)` 决定，
    //! 当前平台仅一条分支被编译，但通过 `DriverKind::as_str` 可覆盖全部枚举变体映射。

    use super::*;

    #[test]
    fn driver_kind_as_str_covers_all_variants() {
        assert_eq!(DriverKind::Landlock.as_str(), "landlock");
        assert_eq!(DriverKind::Seatbelt.as_str(), "seatbelt");
        assert_eq!(DriverKind::WindowsToken.as_str(), "windows-token");
        assert_eq!(DriverKind::Noop.as_str(), "noop");
    }

    #[test]
    fn driver_kind_eq_ord_works() {
        assert_eq!(DriverKind::Landlock, DriverKind::Landlock);
        assert_ne!(DriverKind::Landlock, DriverKind::Noop);
    }

    #[test]
    fn driver_kind_debug_format() {
        let s = format!("{:?}", DriverKind::Landlock);
        assert_eq!(s, "Landlock");
        let s = format!("{:?}", DriverKind::Noop);
        assert_eq!(s, "Noop");
    }

    #[test]
    fn detect_driver_kind_returns_supported_kind_for_current_platform() {
        // 当前平台必须返回一个有效 kind（不 panic），且与 detect_driver 一致
        let kind = detect_driver_kind();
        let driver = detect_driver();
        // 驱动 id 与 kind.as_str 在所有平台应一致
        assert_eq!(driver.id(), kind.as_str());
    }

    #[test]
    fn detect_driver_returns_hardened_only_when_real_driver_active() {
        // 当前平台若降级到 NoopDriver，则 is_hardened 必为 false（C-22）；
        // 若是真实驱动（Landlock/Seatbelt/WindowsToken），is_hardened 为 true。
        let driver = detect_driver();
        let kind = detect_driver_kind();
        match kind {
            DriverKind::Noop => assert!(!driver.is_hardened()),
            DriverKind::Landlock | DriverKind::Seatbelt | DriverKind::WindowsToken => {
                assert!(driver.is_hardened());
            }
        }
    }

    #[test]
    fn detect_driver_post_spawn_default_is_ok() {
        // NoopDriver / Linux/macOS 默认 post_spawn 返回 Ok；Windows 覆写。
        // 这里仅验证不 panic + 返回 Ok 或可忽略错误（取决于平台）。
        let driver = detect_driver();
        let _ = driver.post_spawn(0);
    }

    #[test]
    fn detect_driver_apply_with_default_policy_does_not_panic() {
        // 默认 `WorkspaceWrite` 策略下 apply 应返回 Ok（真实驱动可能在
        // 跨平台非根环境返回错误，但不 panic）。这里仅验证不 panic。
        let driver = detect_driver();
        let policy = minicoding_core::sandbox::SandboxPolicy::default();
        let mut cmd = std::process::Command::new("echo");
        let _ = driver.apply(&policy, &mut cmd);
    }
}
