//! # minicoding-sandbox
//!
//! `OS` 级沙箱驱动：实现 `core::sandbox::SandboxDriver` trait（T-M4-1/2/3）。
//!
//! ## 技术选型
//!
//! 基于 `landlock` crate（Linux Landlock LSM，MIT/Apache-2.0）实现文件系统隔离。
//! **不使用** `sandbox-run`（EUPL-1.2 许可证不合规，违反 AGENTS.md §2.7）——
//! 直接用底层 `landlock` 主流库实现 Linux 驱动，由本 crate 的薄封装层把
//! `SandboxPolicy` 映射到 landlock ruleset（非"自研跨平台沙箱胶水"，底层仍是
//! 主流库，见 `tech-stack.md` §13 选型调整说明）。
//!
//! ## 平台优先级（Linux 先行）
//!
//! - M4：仅实现 Linux（Landlock，内核 5.13+）；旧内核降级 `NoopDriver` + warn；
//! - M5+：补齐 macOS（Seatbelt）；
//! - M6+：补齐 Windows（受限令牌）。
//!
//! `detect_driver()` 编译期按 `cfg!(target_os)` 选实现，运行期探测内核支持。
//! 无可用硬隔离时返回 `NoopDriver`（来自 core）并打 `warn`，依赖容器自身隔离
//! （对应 `ExternalSandbox` 策略，C-22）。
//!
//! 详见 `docs/modules.md` §7、`docs/security.md` §8。

#![deny(clippy::all, clippy::pedantic)]

mod driver;
mod external;
mod hardening;

#[cfg(target_os = "linux")]
mod linux;

pub use driver::{DriverKind, detect_driver, detect_driver_kind};
pub use external::ExternalSandboxDriver;
pub use hardening::{harden_process, vcs_protected_dirs};

/// re-export core 的 trait 与类型，便于调用方单点导入。
pub use minicoding_core::sandbox::{NoopDriver, SandboxDriver, SandboxError, SandboxPolicy};
