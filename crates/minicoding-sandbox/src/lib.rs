//! # minicoding-sandbox
//!
//! `OS` 级沙箱驱动：实现 `core::sandbox::SandboxDriver` trait（T-M4-1/2/3）。
//!
//! ## 技术选型
//!
//! - **Linux**：`landlock` crate（Landlock LSM，MIT/Apache-2.0）——文件系统隔离；
//! - **macOS**：`sandbox_init(3)` FFI（Seatbelt，10.5+ 内置）——文件系统隔离；
//! - **Windows**：`windows-sys` crate（Job Object + UI 限制，MIT/Apache-2.0）——
//!   进程级遏制（文件系统隔离需 AppContainer，作为后续增强）。
//!
//! **不使用** `sandbox-run`（EUPL-1.2 许可证不合规，违反 AGENTS.md §2.7）。
//!
//! ## 平台支持
//!
//! - Linux：Landlock（内核 5.13+），旧内核降级 `NoopDriver` + warn；
//! - macOS：Seatbelt（10.5+），全版本支持；
//! - Windows：Job Object（Vista+），提供进程遏制与 UI 隔离。
//!
//! `detect_driver()` 编译期按 `cfg!(target_os)` 选实现，运行期探测内核支持。
//! 无可用硬隔离时返回 `NoopDriver`（来自 core）并打 `warn`，依赖容器自身隔离
//! （对应 `ExternalSandbox` 策略，C-22）。
//!
//! 详见 `docs/modules.md` §7、`docs/security.md` §8。

#![deny(clippy::all, clippy::pedantic)]

mod denial;
mod driver;
mod external;
mod hardening;

/// Linux Landlock 驱动（`probe_fs_abi`/`net_restriction_supported` 供 doctor
/// 如实报告内核实际能力，SEC-2）。
#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "windows")]
mod windows;

pub use denial::{DenialDetector, PLATFORM_SIGNATURES, SandboxCircuitBreaker};
pub use driver::{DriverKind, detect_driver, detect_driver_kind};
pub use external::ExternalSandboxDriver;
pub use hardening::{harden_process, vcs_protected_dirs};

/// re-export core 的 trait 与类型，便于调用方单点导入。
pub use minicoding_core::sandbox::{
    BreakerState, DenialMatch, DenialSignature, NoopDriver, SandboxDenialDetector,
    SandboxDenialTracker, SandboxDriver, SandboxError, SandboxPolicy,
};
