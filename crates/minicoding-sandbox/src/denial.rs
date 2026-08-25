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
    SandboxDenyKind,
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
        kind_label: "syscall_blocked",
    },
    DenialSignature {
        platform: "any",
        pattern: "Permission denied",
        reason: "EACCES",
        kind_label: "write_forbidden",
    },
    // Linux Landlock / seccomp
    DenialSignature {
        platform: "linux",
        pattern: "landlock",
        reason: "landlock_denied",
        kind_label: "write_forbidden",
    },
    DenialSignature {
        platform: "linux",
        pattern: "Bad system call",
        reason: "seccomp_sigsys",
        kind_label: "syscall_blocked",
    },
    DenialSignature {
        platform: "linux",
        pattern: "SIGSYS",
        reason: "seccomp_sigsys",
        kind_label: "syscall_blocked",
    },
    // macOS Seatbelt
    DenialSignature {
        platform: "macos",
        pattern: "sandbox-exec",
        reason: "seatbelt_denied",
        kind_label: "external",
    },
    DenialSignature {
        platform: "macos",
        pattern: "Sandbox violation",
        reason: "seatbelt_violation",
        kind_label: "write_forbidden",
    },
    // Windows
    DenialSignature {
        platform: "windows",
        pattern: "Access is denied",
        reason: "windows_access_denied",
        kind_label: "write_forbidden",
    },
    DenialSignature {
        platform: "windows",
        pattern: "privilege not held",
        reason: "windows_privilege_not_held",
        kind_label: "external",
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
        // S-6：Runtime 合成的 errno 标记优先判定权威性。`\x01`/`\x02` 控制字符
        // 序列无法被子进程 stderr 输出可靠伪造，且标记由 Runtime 在进程内合成
        // （仅当错误携带结构化 EPERM/EACCES 时追加）——命中即内核级硬反馈；
        // 仅传统文本模式命中的为 advisory（可能来自业务失败或提示注入伪造）。
        let authoritative =
            error_text.contains(minicoding_core::sandbox::DENIED_ERRNO_MARKER_PREFIX);
        for sig in PLATFORM_SIGNATURES {
            if error_text.contains(sig.pattern) {
                return Some(DenialMatch {
                    signature: *sig,
                    tool: tool.to_string(),
                    kind: deny_kind_from_label(sig.kind_label, sig.reason, error_text),
                    authoritative,
                });
            }
        }
        None
    }
}

/// 把签名表标签映射为结构化 `SandboxDenyKind`（M-09）。
///
/// payload 尽力提取：`syscall_blocked` 取 stderr 首行（如 `Bad system call`），
/// 其余无法从文本可靠解析的字段留空——完整原文在 `ToolResultMeta.sandbox_denied.detail`。
fn deny_kind_from_label(label: &str, reason: &str, error_text: &str) -> SandboxDenyKind {
    let first_line = || {
        error_text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(120)
            .collect::<String>()
    };
    match label {
        "syscall_blocked" => SandboxDenyKind::SyscallBlocked {
            syscall: first_line(),
        },
        "write_forbidden" => SandboxDenyKind::WriteForbidden {
            path: String::new(),
        },
        "resource_limit" => SandboxDenyKind::ResourceLimit {
            limit: reason.to_string(),
        },
        _ => SandboxDenyKind::External,
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
        // S-6：纯文本命中为 advisory（不计熔断）
        assert!(!m.authoritative, "纯文本命中应为 advisory");
    }

    #[test]
    fn detect_runtime_errno_marker_is_authoritative() {
        // S-6：Runtime 合成标记命中 → authoritative=true（内核级硬反馈）。
        // 模拟 build_denial_result 组装的检测文本：原始错误 + 标记行。
        use minicoding_core::sandbox::{DENIED_ERRNO_MARKER_PREFIX, DENIED_ERRNO_MARKER_SUFFIX};
        let d = DenialDetector::new();
        let text = format!(
            "io: Operation not permitted\n{PREFIX}1{SUFFIX}",
            PREFIX = DENIED_ERRNO_MARKER_PREFIX,
            SUFFIX = DENIED_ERRNO_MARKER_SUFFIX
        );
        let m = d.detect("fs.write", &text).unwrap();
        assert!(m.authoritative, "errno 标记命中应为 authoritative");

        // 同一签名，无标记 → advisory
        let m = d.detect("fs.write", "io: Operation not permitted").unwrap();
        assert!(!m.authoritative);
    }

    #[test]
    fn forged_marker_without_control_chars_is_not_authoritative() {
        // S-6 防伪：子进程输出若试图打印字面量 "MINICODING_DENIED_ERRNO=1"，
        // 缺少 \x01 控制字符前缀则不构成权威判定
        let d = DenialDetector::new();
        let text = "echo MINICODING_DENIED_ERRNO=1 done\nOperation not permitted";
        let m = d.detect("shell.run", text).unwrap();
        assert!(!m.authoritative);
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
    fn detect_returns_structured_kind() {
        // M-09：detect 产出结构化 SandboxDenyKind（透传 ToolResultMeta.sandbox_denied）
        let d = DenialDetector::new();
        let m = d
            .detect("shell.run", "Bad system call (core dumped)")
            .unwrap();
        assert_eq!(
            m.kind,
            SandboxDenyKind::SyscallBlocked {
                syscall: "Bad system call (core dumped)".into()
            }
        );
        let m = d
            .detect("shell.run", "landlock: operation not permitted")
            .unwrap();
        assert_eq!(
            m.kind,
            SandboxDenyKind::WriteForbidden {
                path: String::new()
            }
        );
        let m = d.detect("shell.run", "sandbox-exec: fatal error").unwrap();
        assert_eq!(m.kind, SandboxDenyKind::External);
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
