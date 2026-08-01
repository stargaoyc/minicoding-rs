//! 沙箱拒绝检测与熔断（T-M4-5，参考 `security.md` §8.7/§8.8）。
//!
//! 命令在沙箱内失败时，错误可能来自"业务逻辑"或"沙箱拒绝"，二者处理方式不同。
//! 本模块提供：
//!
//! - [`DenialDetector`]：denial 签名库（按平台匹配 `errno`/stderr 关键字），把沙箱
//!   拒绝从普通错误中识别出来；
//! - [`SandboxCircuitBreaker`]：单 turn 内的拒绝计数器，达 3 次注入 system
//!   reminder，达 5 次强制 `TurnEnd`（C-30：不可被 LLM 绕过）。
//!
//! ## 升级流（`security.md` §8.7）
//!
//! 识别为 `sandbox_denied` 后由 `Runtime` 生成 `PermissionPrompt`，用户批准则放宽
//! 策略重试（仍受 L0 黑名单约束）。本模块仅做"识别"+"熔断"，升级交互在 `Runtime`
//! 内组合 `PermissionPrompter` 完成。
//!
//! ## 设计取舍
//!
//! 不引入 `regex`（core 依赖约束），用纯字符串 `contains` 匹配 errno 与 stderr
//! 关键字。平台签名表为静态常量，新增平台仅需扩展 `PLATFORM_SIGNATURES`。

/// 单条 denial 签名（匹配 stderr 子串或 errno 文本）。
#[derive(Debug, Clone, Copy)]
pub struct DenialSignature {
    /// 平台名（`linux`/`macos`/`windows`，仅供诊断）。
    pub platform: &'static str,
    /// stderr 子串（大小写敏感）。
    pub pattern: &'static str,
    /// 简短说明（落审计用）。
    pub reason: &'static str,
}

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

/// 拒绝检测结果。
#[derive(Debug, Clone)]
pub struct DenialMatch {
    /// 命中的签名（供审计/诊断）。
    pub signature: DenialSignature,
    /// 工具名（`shell.run`/`fs.write` 等）。
    pub tool: String,
}

/// 沙箱拒绝检测器（无状态，可共享）。
///
/// `detect` 对 stderr/errno 文本做子串匹配，命中任一签名返回 `DenialMatch`。
/// 不引入正则（core 依赖约束），用纯 `str::contains` 足以覆盖签名库的关键字
/// 场景；如未来需要更复杂匹配，可在 `minicoding-sandbox` 提供正则实现。
#[derive(Debug, Default, Clone, Copy)]
pub struct DenialDetector;

impl DenialDetector {
    /// 创建检测器。
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// 检测错误文本是否为沙箱拒绝。
    ///
    /// `error_text` 应为 `io::Error::to_string()` 或 stderr 合并文本。命中返回
    /// `DenialMatch`，未命中返回 `None`（普通错误）。
    #[must_use]
    pub fn detect(&self, tool: &str, error_text: &str) -> Option<DenialMatch> {
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

/// 沙箱拒绝熔断状态（单 turn 内有效，turn 结束重置）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BreakerState {
    /// 未触发熔断（计数 < `soft_threshold`）。
    #[default]
    Closed,
    /// 软熔断（计数 ≥ `soft_threshold`，注入提醒但仍允许继续）。
    SoftTripped,
    /// 硬熔断（计数 ≥ `hard_threshold`，强制 `TurnEnd`）。
    HardTripped,
}

/// 沙箱拒绝熔断器（C-30：不可被 LLM 绕过）。
///
/// 单 turn 内累计 `sandbox_denied` 计数：
/// - `< soft_threshold`（默认 3）：正常升级流；
/// - `≥ soft_threshold`：注入 system reminder 提醒方向有误；
/// - `≥ hard_threshold`（默认 5）：强制 `TurnEnd`，回灌错误总结。
///
/// 由 `Runtime` 持有（`Mutex` 保护），turn 结束时 `reset`。熔断阈值可配
///（`[sandbox] denial_threshold` / `hard_threshold`）。
pub struct SandboxCircuitBreaker {
    count: std::sync::Mutex<usize>,
    soft_threshold: usize,
    hard_threshold: usize,
}

impl std::fmt::Debug for SandboxCircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.count.lock().map_or(0, |c| *c);
        f.debug_struct("SandboxCircuitBreaker")
            .field("count", &count)
            .field("soft_threshold", &self.soft_threshold)
            .field("hard_threshold", &self.hard_threshold)
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
            count: std::sync::Mutex::new(0),
            soft_threshold,
            hard_threshold,
        }
    }

    /// 默认阈值（3/5）。
    #[must_use]
    pub fn default_thresholds() -> Self {
        Self::new(3, 5)
    }

    /// 记录一次 `sandbox_denied`，返回记录后的状态（用于触发提醒/`TurnEnd`）。
    ///
    /// 锁中毒视为已硬熔断（保守，C-30：宁可中止也不放行）。
    pub fn record_denial(&self) -> BreakerState {
        let Ok(mut count) = self.count.lock() else {
            tracing::error!("circuit breaker lock poisoned, force hard trip");
            return BreakerState::HardTripped;
        };
        *count += 1;
        let c = *count;
        drop(count);
        if c >= self.hard_threshold {
            BreakerState::HardTripped
        } else if c >= self.soft_threshold {
            BreakerState::SoftTripped
        } else {
            BreakerState::Closed
        }
    }

    /// 当前状态（不增加计数）。
    pub fn state(&self) -> BreakerState {
        let c = self.count.lock().map_or(self.hard_threshold, |g| *g);
        if c >= self.hard_threshold {
            BreakerState::HardTripped
        } else if c >= self.soft_threshold {
            BreakerState::SoftTripped
        } else {
            BreakerState::Closed
        }
    }

    /// 重置计数（turn 结束时调用）。
    pub fn reset(&self) {
        if let Ok(mut g) = self.count.lock() {
            *g = 0;
        }
    }

    /// 当前计数（诊断用，不增加）。
    pub fn count(&self) -> usize {
        self.count.lock().map_or(0, |g| *g)
    }
}

/// 软熔断提醒文案（注入 system reminder，`security.md` §8.8）。
#[must_use]
pub fn soft_trip_reminder(count: usize) -> String {
    format!(
        "连续 {count} 次沙箱拒绝，可能方向有误。请重新评估任务可行性\
         或向用户确认是否切换到更宽松的沙箱预设。"
    )
}

/// 硬熔断错误总结（回灌 LLM 与用户）。
#[must_use]
pub fn hard_trip_summary(count: usize) -> String {
    format!(
        "[沙箱拒绝熔断] 连续 {count} 次沙箱拒绝，已强制终止本轮。\
         请检查任务是否需要更宽松的沙箱预设或人工介入。"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

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
    fn reminder_text_contains_count() {
        assert!(soft_trip_reminder(3).contains('3'));
        assert!(hard_trip_summary(5).contains('5'));
    }
}
