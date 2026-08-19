//! 沙箱拒绝检测与熔断的领域实现（M-05 从 core 下沉）。
//!
//! 实现 `core::sandbox` 的 `SandboxDenialDetector`（平台签名库）与
//! `SandboxDenialTracker`（单 turn 熔断，C-30）。core 只依赖 trait 抽象，
//! 本 crate 提供完整实现；未启用本 crate 时 `RuntimeBuilder` 默认注入 core 的
//! `NoopDenialDetector`/`NoopDenialTracker` 兜底。
//!
//! 与 core 兜底实现的差异：本模块持有平台签名表 `PLATFORM_SIGNATURES`（领域
//! 知识），并可在未来引入 `regex` 做更强签名匹配（core 因依赖约束不引入 regex）。

use minicoding_core::sandbox::{
    BreakerState, DenialMatch, DenialSignature, SandboxDenialDetector, SandboxDenialTracker,
};

/// 跨平台 denial 签名库（按 `security.md` §8.7）。
///
/// 命中任一签名即判定为 `sandbox_denied`。`EPERM`/`EACCES` 文本来自 Rust 的
/// `io::Error` `to_string()`（`Operation not permitted`/`Permission denied`），
/// 覆盖 Linux/macOS/Windows 共通场景；Landlock/seccomp/Seatbelt 特定关键字
/// 用于内核级硬反馈。
pub const PLATFORM_SIGNATURES: &[DenialSignature] = &[
    // 通用 errno 文本（Rust io::Error 渲染）
    DenialSignature {
        platform: "any",
        pattern: "Operation not permitted",
        reason: "EPERM",
    },
    DenialSignature {
        platform: "any",
        pattern: "Permission denied",
        reason: "EACCES",
    },
    // Linux Landlock / seccomp
    DenialSignature {
        platform: "linux",
        pattern: "landlock",
        reason: "landlock_denied",
    },
    DenialSignature {
        platform: "linux",
        pattern: "Bad system call",
        reason: "seccomp_sigsys",
    },
    DenialSignature {
        platform: "linux",
        pattern: "SIGSYS",
        reason: "seccomp_sigsys",
    },
    // macOS Seatbelt
    DenialSignature {
        platform: "macos",
        pattern: "sandbox-exec",
        reason: "seatbelt_denied",
    },
    DenialSignature {
        platform: "macos",
        pattern: "Sandbox violation",
        reason: "seatbelt_violation",
    },
    // Windows
    DenialSignature {
        platform: "windows",
        pattern: "Access is denied",
        reason: "windows_access_denied",
    },
    DenialSignature {
        platform: "windows",
        pattern: "privilege not held",
        reason: "windows_privilege_not_held",
    },
];

/// 沙箱拒绝检测器（无状态，可共享）。
///
/// `detect` 对 stderr/errno 文本做子串匹配，命中任一签名返回 `DenialMatch`。
/// 实现 `core` 的 `SandboxDenialDetector` trait（M-05 下沉）。
#[derive(Debug, Default, Clone, Copy)]
pub struct DenialDetector;

impl DenialDetector {
    /// 创建检测器。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl SandboxDenialDetector for DenialDetector {
    fn detect(&self, tool: &str, error_text: &str) -> Option<DenialMatch> {
        for sig in PLATFORM_SIGNATURES {
            if error_text.contains(sig.pattern) {
                return Some(DenialMatch {
                    signature: *sig,
                    tool: tool.to_string(),
                });
            }
        }
        None
    }
}

/// 沙箱拒绝熔断器（C-30：不可被 LLM 绕过）。
///
/// 单 turn 内累计 `sandbox_denied` 计数：
/// - `< soft_threshold`（默认 3）：正常升级流；
/// - `≥ soft_threshold`：注入 system reminder 提醒方向有误；
/// - `≥ hard_threshold`（默认 5）：强制 `TurnEnd`，回灌错误总结。
///
/// 实现 `core` 的 `SandboxDenialTracker` trait。`Runtime` 经 `RuntimeBuilder`
/// 注入（有沙箱功能时用本实现，否则用 core 的 `NoopDenialTracker` 兜底）。
/// 计数逻辑复用 core 的 `util::CircuitBreaker` 通用骨架（M-05 熔断去重）。
pub struct SandboxCircuitBreaker {
    inner: std::sync::Mutex<minicoding_core::util::CircuitBreaker>,
}

impl std::fmt::Debug for SandboxCircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.count();
        f.debug_struct("SandboxCircuitBreaker")
            .field("count", &count)
            .finish()
    }
}

impl SandboxCircuitBreaker {
    /// 创建熔断器。
    ///
    /// `soft_threshold` 默认 3，`hard_threshold` 默认 5（见 `security.md` §8.8）。
    #[must_use]
    pub fn new(soft_threshold: usize, hard_threshold: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(minicoding_core::util::CircuitBreaker::with_config(
                minicoding_core::util::CircuitBreakerConfig {
                    soft_threshold,
                    hard_threshold,
                },
            )),
        }
    }

    /// 默认阈值（3/5）。
    #[must_use]
    pub fn default_thresholds() -> Self {
        Self::new(3, 5)
    }
}

impl SandboxDenialTracker for SandboxCircuitBreaker {
    fn record_denial(&self) -> BreakerState {
        let Ok(mut inner) = self.inner.lock() else {
            tracing::error!("circuit breaker lock poisoned, force hard trip");
            return BreakerState::HardTripped;
        };
        inner.record()
    }

    fn state(&self) -> BreakerState {
        self.inner
            .lock()
            .map_or(BreakerState::HardTripped, |g| g.state())
    }

    fn reset(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.reset();
        }
    }

    fn count(&self) -> usize {
        self.inner.lock().map_or(0, |g| g.count())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use std::sync::Arc;

    #[test]
    fn detect_eperm() {
        let d = DenialDetector::new();
        let m = d
            .detect("shell.run", "exit code: 1\nOperation not permitted")
            .unwrap();
        assert_eq!(m.signature.reason, "EPERM");
        assert_eq!(m.tool, "shell.run");
    }

    #[test]
    fn detect_landlock() {
        let d = DenialDetector::new();
        let m = d.detect("fs.write", "landlock denied write").unwrap();
        assert_eq!(m.signature.reason, "landlock_denied");
    }

    #[test]
    fn detect_seatbelt() {
        let d = DenialDetector::new();
        let m = d.detect("shell.run", "sandbox-exec: deny").unwrap();
        assert_eq!(m.signature.platform, "macos");
    }

    #[test]
    fn detect_windows() {
        let d = DenialDetector::new();
        assert!(d.detect("fs.write", "Error: Access is denied").is_some());
    }

    #[test]
    fn detect_not_denial() {
        let d = DenialDetector::new();
        assert!(
            d.detect("shell.run", "exit code: 1\nfile not found")
                .is_none()
        );
    }

    #[test]
    fn breaker_soft_trip_at_3() {
        let b = SandboxCircuitBreaker::new(3, 5);
        assert_eq!(b.record_denial(), BreakerState::Closed);
        assert_eq!(b.record_denial(), BreakerState::Closed);
        assert_eq!(b.record_denial(), BreakerState::SoftTripped);
        assert_eq!(b.state(), BreakerState::SoftTripped);
    }

    #[test]
    fn breaker_hard_trip_at_5() {
        let b = SandboxCircuitBreaker::default_thresholds();
        for _ in 0..4 {
            let _ = b.record_denial();
        }
        assert_eq!(b.record_denial(), BreakerState::HardTripped);
        assert_eq!(b.state(), BreakerState::HardTripped);
    }

    #[test]
    fn breaker_reset() {
        let b = SandboxCircuitBreaker::default_thresholds();
        for _ in 0..3 {
            let _ = b.record_denial();
        }
        assert_eq!(b.state(), BreakerState::SoftTripped);
        b.reset();
        assert_eq!(b.state(), BreakerState::Closed);
        assert_eq!(b.count(), 0);
    }

    #[test]
    fn trait_object_usage() {
        // 验证对象安全：trait 可作 dyn 对象（Runtime 经 Arc<dyn> 持有）
        let tracker: Arc<dyn SandboxDenialTracker> =
            Arc::new(SandboxCircuitBreaker::default_thresholds());
        assert_eq!(tracker.state(), BreakerState::Closed);
        assert_eq!(tracker.record_denial(), BreakerState::Closed);
        let detector: Arc<dyn SandboxDenialDetector> = Arc::new(DenialDetector::new());
        assert!(detector.detect("fs.read", "landlock denied read").is_some());
    }
}
