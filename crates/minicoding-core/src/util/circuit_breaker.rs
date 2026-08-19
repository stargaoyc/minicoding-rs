//! 通用熔断器骨架（M-05 熔断去重）。
//!
//! 沙箱拒绝熔断（C-30，`minicoding-sandbox::SandboxCircuitBreaker` / core 兜底
//! `NoopDenialTracker`）与上下文压缩熔断（C-29，`minicoding-context` 的
//! `CircuitBreaker`）此前各自实现了"计数 + 双阈值"逻辑，重复。本模块抽出通用
//! 骨架：**单计数器 + 双阈值 + 三态状态**，两处领域实现复用，各自保留领域状态
//! 映射（沙箱映射为 Closed/SoftTripped/HardTripped，压缩把 `fail` 计数复用骨架、
//! 另维护 thrash 计数器）。
//!
//! 骨架为纯数据（`&mut` API + `Clone`/`Debug`），由领域层自行用
//! `std::sync::Mutex`/`tokio::sync::Mutex` 适配 `&self` 的 trait 接口
//! （如 `SandboxDenialTracker`）。

/// 熔断器配置（双阈值）。
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreakerConfig {
    /// 软阈值：计数达到后进入 `SoftTripped`（注入提醒，仍允许继续）。
    pub soft_threshold: usize,
    /// 硬阈值：计数达到后进入 `HardTripped`（强制中止）。
    pub hard_threshold: usize,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            soft_threshold: 3,
            hard_threshold: 5,
        }
    }
}

/// 熔断状态（与领域状态语义正交，领域层自行映射）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BreakerState {
    /// 未熔断（计数 < 软阈值）。
    #[default]
    Closed,
    /// 软熔断（计数 ≥ 软阈值）。
    SoftTripped,
    /// 硬熔断（计数 ≥ 硬阈值）。
    HardTripped,
}

/// 通用熔断器骨架（单计数器 + 双阈值，纯数据）。
///
/// `record()` 递增计数并返回当前状态；`reset()` 清零；`state()`/`count()` 只读。
/// 计数用 `saturating_add` 防溢出。
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    soft_threshold: usize,
    hard_threshold: usize,
    count: usize,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker {
    /// 默认阈值（soft=3, hard=5，与 `security.md` §8.8 / `design.md` §3.6 一致）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(CircuitBreakerConfig::default())
    }

    /// 指定阈值的熔断器。
    #[must_use]
    pub fn with_config(config: CircuitBreakerConfig) -> Self {
        Self {
            soft_threshold: config.soft_threshold,
            hard_threshold: config.hard_threshold,
            count: 0,
        }
    }

    /// 记录一次失败并返回新状态。
    #[must_use]
    pub fn record(&mut self) -> BreakerState {
        self.count = self.count.saturating_add(1);
        self.state()
    }

    /// 当前状态（不增计数）。
    #[must_use]
    pub fn state(&self) -> BreakerState {
        if self.count >= self.hard_threshold {
            BreakerState::HardTripped
        } else if self.count >= self.soft_threshold {
            BreakerState::SoftTripped
        } else {
            BreakerState::Closed
        }
    }

    /// 重置计数。
    pub fn reset(&mut self) {
        self.count = 0;
    }

    /// 当前计数（不增计数）。
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_clean() {
        let cb = CircuitBreaker::new();
        assert_eq!(cb.count(), 0);
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn soft_trip_at_soft_threshold() {
        let mut cb = CircuitBreaker::new(); // 3/5
        assert_eq!(cb.record(), BreakerState::Closed);
        assert_eq!(cb.record(), BreakerState::Closed);
        assert_eq!(cb.record(), BreakerState::SoftTripped);
        assert_eq!(cb.state(), BreakerState::SoftTripped);
    }

    #[test]
    fn hard_trip_at_hard_threshold() {
        let mut cb = CircuitBreaker::new(); // 3/5
        for _ in 0..4 {
            let _ = cb.record();
        }
        assert_eq!(cb.record(), BreakerState::HardTripped);
        assert_eq!(cb.state(), BreakerState::HardTripped);
    }

    #[test]
    fn reset_clears_count() {
        let mut cb = CircuitBreaker::new();
        for _ in 0..5 {
            let _ = cb.record();
        }
        assert_eq!(cb.state(), BreakerState::HardTripped);
        cb.reset();
        assert_eq!(cb.state(), BreakerState::Closed);
        assert_eq!(cb.count(), 0);
    }

    #[test]
    fn custom_thresholds() {
        let mut cb = CircuitBreaker::with_config(CircuitBreakerConfig {
            soft_threshold: 1,
            hard_threshold: 2,
        });
        assert_eq!(cb.record(), BreakerState::SoftTripped);
        assert_eq!(cb.record(), BreakerState::HardTripped);
    }

    #[test]
    fn saturating_count_never_underflows_or_overflows() {
        let mut cb = CircuitBreaker::new();
        for _ in 0..1000 {
            let _ = cb.record();
        }
        assert_eq!(cb.state(), BreakerState::HardTripped);
        assert!(cb.count() >= 5);
    }
}
