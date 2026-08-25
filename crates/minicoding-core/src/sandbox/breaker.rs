//! 沙箱拒绝检测与熔断的抽象层（T-M4-5，`security.md` §8.7/§8.8）。
//!
//! 命令在沙箱内失败时，错误可能来自"业务逻辑"或"沙箱拒绝"，二者处理方式不同。
//! 本模块提供：
//!
//! - 数据：`BreakerState`（M-05 后 re-export 自 `util::circuit_breaker` 通用骨架）
//!   / `DenialSignature` / `DenialMatch`（无算法）；
//! - 抽象：`SandboxDenialDetector`（把错误文本识别为沙箱拒绝）与
//!   `SandboxDenialTracker`（单 turn 内拒绝计数熔断，C-30）；
//! - 兜底：`NoopDenialDetector`（永不匹配）与 `NoopDenialTracker`（仅计数，
//!   无领域签名库）——供 `RuntimeBuilder` 默认注入（与 `NoopDriver` 同哲学，
//!   见 AGENTS.md §3.3/§3.4）。
//!
//! 领域实现（平台签名库、增强熔断）在 `minicoding-sandbox`（M-05 下沉，core
//! 不保留领域算法，不引入 `regex`）。计数逻辑复用 `util::CircuitBreaker`
//! 通用骨架（M-05 熔断去重，与压缩熔断共用）。

use crate::sandbox::SandboxDenyKind;
use crate::util::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};

/// Runtime 合成的权威 denial 标记行前缀（S-6）。
///
/// `build_denial_result` 在错误携带结构化 errno（EPERM/EACCES 的
/// `raw_os_error`）时，向检测文本追加 `\x01MINICODING_DENIED_ERRNO={n}\x02`
/// 标记行：`\x01`/`\x02` 是控制字符，子进程 stderr 输出无法可靠伪造（终端/
/// shell 会转义或剥离），且标记由 Runtime 在进程内合成——检测器命中该标记
/// 即可判定为**权威**沙箱拒绝；仅文本模式命中的为 advisory。
pub const DENIED_ERRNO_MARKER_PREFIX: &str = "\x01MINICODING_DENIED_ERRNO=";

/// 权威 denial 标记行的结束字符（S-6，见 [`DENIED_ERRNO_MARKER_PREFIX`]）。
pub const DENIED_ERRNO_MARKER_SUFFIX: &str = "\x02";

/// 单条 denial 签名（匹配 stderr 子串或 errno 文本）。
#[derive(Debug, Clone, Copy)]
pub struct DenialSignature {
    /// 平台名（`linux`/`macos`/`windows`，仅供诊断）。
    pub platform: &'static str,
    /// stderr 子串（大小写敏感）。
    pub pattern: &'static str,
    /// 简短说明（落审计用）。
    pub reason: &'static str,
    /// 拒绝类型标签（`syscall_blocked`/`write_forbidden`/`resource_limit`/`external`），
    /// 由检测实现映射为结构化 `SandboxDenyKind`（M-09，带 payload 的枚举无法入静态表）。
    pub kind_label: &'static str,
}

/// 拒绝检测结果。
#[derive(Debug, Clone)]
pub struct DenialMatch {
    /// 命中的签名（供审计/诊断）。
    pub signature: DenialSignature,
    /// 工具名（`shell.run`/`fs.write` 等）。
    pub tool: String,
    /// 结构化拒绝类型（M-09，透传到 `ToolResultMeta.sandbox_denied`）。
    pub kind: SandboxDenyKind,
    /// 是否为**权威**判定（S-6）：
    /// - `true`：检测文本含 Runtime 合成的 errno 标记（内核级硬反馈），计入
    ///   熔断（C-30）；
    /// - `false`：advisory——仅文本启发式命中，可能来自业务逻辑失败或提示注入
    ///   伪造的输出，返回提示性结果但**不计熔断**。
    pub authoritative: bool,
}

/// 沙箱拒绝熔断状态（单 turn 内有效，turn 结束重置）。
///
/// M-05 熔断去重后定义于 `util::circuit_breaker`，此处 re-export 保持
/// `core::sandbox::BreakerState` 路径兼容（`rt.rs`/`minicoding-sandbox` 引用）。
pub use crate::util::circuit_breaker::BreakerState;

/// 沙箱拒绝检测器抽象：把工具错误文本识别为沙箱拒绝。
///
/// 实现由 `minicoding-sandbox` 提供（平台签名库）；core 的 `Runtime` 只依赖此
/// trait，不接触具体实现（M-05 下沉，避免 core 含领域签名匹配算法）。
pub trait SandboxDenialDetector: Send + Sync {
    /// 检测错误文本是否为沙箱拒绝。命中返回 `DenialMatch`，未命中返回 `None`。
    fn detect(&self, tool: &str, error_text: &str) -> Option<DenialMatch>;
}

/// 沙箱拒绝熔断跟踪抽象（C-30：不可被 LLM 绕过）。
///
/// 单 turn 内累计 `sandbox_denied` 计数：达软阈值注入提醒、达硬阈值强制
/// `TurnEnd`。`Runtime` 经 `RuntimeBuilder` 注入具体实现（默认 `NoopDenialTracker`
/// 仅计数，无领域签名库）。
pub trait SandboxDenialTracker: Send + Sync {
    /// 记录一次 `sandbox_denied`，返回记录后的状态。
    fn record_denial(&self) -> BreakerState;
    /// 当前状态（不增加计数）。
    fn state(&self) -> BreakerState;
    /// 重置计数（turn 结束时调用）。
    fn reset(&self);
    /// 当前计数（诊断用，不增加）。
    fn count(&self) -> usize;
}

/// 永不匹配的默认检测器（无沙箱功能时注入；行为与 `NoopDriver` 一致）。
#[derive(Debug, Default)]
pub struct NoopDenialDetector;

impl SandboxDenialDetector for NoopDenialDetector {
    fn detect(&self, _tool: &str, _error_text: &str) -> Option<DenialMatch> {
        None
    }
}

/// 默认熔断器（仅计数，无领域签名库；供 `RuntimeBuilder` 默认注入）。
///
/// 内部复用 `util::CircuitBreaker` 通用骨架（M-05 熔断去重）；与
/// `minicoding-sandbox` 的 `SandboxCircuitBreaker` 逻辑相同，后者在领域 crate
/// 中可组合 `DenialDetector` 做增强（如正则匹配）。语义与 C-30 一致。
#[derive(Debug)]
pub struct NoopDenialTracker {
    inner: std::sync::Mutex<CircuitBreaker>,
}

impl NoopDenialTracker {
    /// 创建熔断器。`soft_threshold` 默认 3，`hard_threshold` 默认 5
    /// （见 `security.md` §8.8）。
    #[must_use]
    pub fn new(soft_threshold: usize, hard_threshold: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(CircuitBreaker::with_config(CircuitBreakerConfig {
                soft_threshold,
                hard_threshold,
            })),
        }
    }

    /// 默认阈值（3/5）。
    #[must_use]
    pub fn default_thresholds() -> Self {
        Self::new(3, 5)
    }
}

impl SandboxDenialTracker for NoopDenialTracker {
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
    fn noop_detector_never_matches() {
        let d = NoopDenialDetector;
        assert!(d.detect("shell.run", "Operation not permitted").is_none());
    }

    #[test]
    fn noop_tracker_soft_trip_at_3() {
        let b = NoopDenialTracker::new(3, 5);
        assert_eq!(b.record_denial(), BreakerState::Closed);
        assert_eq!(b.record_denial(), BreakerState::Closed);
        assert_eq!(b.record_denial(), BreakerState::SoftTripped);
        assert_eq!(b.state(), BreakerState::SoftTripped);
    }

    #[test]
    fn noop_tracker_hard_trip_at_5() {
        let b = NoopDenialTracker::default_thresholds();
        for _ in 0..4 {
            let _ = b.record_denial();
        }
        assert_eq!(b.record_denial(), BreakerState::HardTripped);
        assert_eq!(b.state(), BreakerState::HardTripped);
    }

    #[test]
    fn noop_tracker_reset() {
        let b = NoopDenialTracker::default_thresholds();
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
