//! 沙箱驱动探测与调度（T-M4-1/2）。
//!
//! `detect_driver()` 编译期按 `cfg!(target_os)` 选实现，运行期探测内核支持：
//! - Linux：探测 Landlock 可用性，可用返回 `LandlockDriver`，否则降级 `NoopDriver` + warn；
//! - macOS/Windows：M4 降级 `NoopDriver` + warn（平台优先级 M5+/M6+ 补齐）。

use minicoding_core::sandbox::{NoopDriver, SandboxDriver};

/// 探测到的驱动类型（供 `doctor --security` 报告，见 T-M4-10）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    /// Linux Landlock LSM（内核 5.13+，硬隔离）。
    Landlock,
    /// macOS Seatbelt（M5+ 补齐，M4 降级）。
    Seatbelt,
    /// Windows 受限令牌（M6+ 补齐，M4 降级）。
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
/// 返回 `Box<dyn SandboxDriver>` 供 `RuntimeBuilder` 注入。Linux 内核支持 Landlock
/// 时返回 `LandlockDriver`（`is_hardened() == true`）；否则降级 `NoopDriver` + warn
/// （C-22：降级需显式声明，`is_hardened()` 如实返回 `false`）。
///
/// macOS/Windows 在 M4 降级 `NoopDriver`（平台优先级 M5+/M6+）。
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
        tracing::warn!(
            driver = "noop",
            reason = "macOS Seatbelt 实现推迟到 M5+（平台优先级），M4 降级 NoopDriver"
        );
        return Box::new(NoopDriver);
    }

    #[cfg(target_os = "windows")]
    {
        tracing::warn!(
            driver = "noop",
            reason = "Windows 受限令牌实现推迟到 M6+（平台优先级），M4 降级 NoopDriver"
        );
        return Box::new(NoopDriver);
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
        return DriverKind::Noop;
    }

    #[cfg(target_os = "windows")]
    {
        return DriverKind::Noop;
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        DriverKind::Noop
    }
}
