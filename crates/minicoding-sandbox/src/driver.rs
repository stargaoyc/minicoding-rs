//! 沙箱驱动探测与调度（T-M4-1/2）。
//!
//! `detect_driver()` 编译期按 `cfg!(target_os)` 选实现，运行期探测内核支持：
//! - Linux：探测 Landlock 可用性，可用返回 `LandlockDriver`，否则降级 `NoopDriver` + warn；
//! - macOS：Seatbelt（10.5+ 全版本支持），返回 `SeatbeltDriver`；
//! - Windows：Job Object（Vista+），返回 `WindowsJobDriver`。

use minicoding_core::sandbox::{NoopDriver, SandboxDriver};

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
