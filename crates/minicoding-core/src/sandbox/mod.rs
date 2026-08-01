//! Sandbox 模块 re-export。

pub mod denial;
mod r#trait;

pub use denial::{
    BreakerState, DenialDetector, DenialMatch, DenialSignature, SandboxCircuitBreaker,
    hard_trip_summary, soft_trip_reminder,
};
pub use r#trait::{NoopDriver, SandboxDriver, SandboxError, SandboxPolicy};
