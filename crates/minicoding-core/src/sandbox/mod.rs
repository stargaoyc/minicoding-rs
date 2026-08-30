//! Sandbox 模块 re-export。
//!
//! core 只保留抽象 trait 与数据（`SandboxDenialDetector`/`SandboxDenialTracker`/
//! `BreakerState` 等）与兜底实现（`NoopDriver`/`NoopDenialTracker`）；领域实现
//! 在 `minicoding-sandbox`（M-05 下沉，`SandboxDriver` 的 OS 沙箱 + denial 签名库）。

mod breaker;
mod r#trait;

pub use breaker::{
    BreakerState, DENIED_ERRNO_MARKER_PREFIX, DENIED_ERRNO_MARKER_SUFFIX, DenialMatch,
    DenialSignature, NoopDenialDetector, NoopDenialTracker, SandboxDenialDetector,
    SandboxDenialTracker, hard_trip_summary, soft_trip_reminder,
};
pub use r#trait::{
    NoopDriver, SandboxDenyKind, SandboxDriver, SandboxError, SandboxPolicy, SpawnHandle,
};
