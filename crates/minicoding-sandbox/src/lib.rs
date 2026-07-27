//! # minicoding-sandbox
//!
//! `OS` 级沙箱驱动：实现 `core::sandbox::SandboxDriver` trait。
//!
//! 基于 `sandbox-run` + `landlock` + `libseccomp` 主流库提供跨平台内核级隔离，
//! **不自研**沙箱胶水代码（见 `tech-stack.md` §11、§13）。
//!
//! ## 平台检测策略
//!
//! `detect_driver()` 编译期按 `cfg!(target_os)` 选实现，运行期探测内核支持。
//! 无可用硬隔离时返回 `NoopDriver`（来自 core）并打 `warn`，依赖容器自身隔离
//! （对应 `ExternalSandbox` 策略）。
//!
//! ## 平台优先级（Linux 先行）
//!
//! - M0-M4：仅实现 Linux（`sandbox-run` + `landlock` + `libseccomp`）；
//! - M5+：补齐 macOS `sandbox-run`（Seatbelt）；
//! - M6+：补齐 Windows 受限令牌 + Job Object。
//!
//! 当前 M0 阶段：仅占位骨架（T-M0-1），`detect_driver()` 实现见 T-M0-7。
//!
//! 详见 `docs/modules.md` §7、`docs/security.md` §8。

#![deny(clippy::all, clippy::pedantic)]
