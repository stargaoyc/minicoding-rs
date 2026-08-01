//! 压缩熔断状态机（见 `docs/design.md` §3.6）。
//!
//! 压缩管道（§3.3）最危险的失效模式是 **Thrash Loop**：压缩后立即又填满 →
//! 再次压缩 → 再填满，烧光 token 预算且不产生有效输出。`CircuitBreaker` 维护
//! 压缩失败计数与连续超阈值计数，在达到阈值时熔断中止本轮（C-29：熔断不可被
//! LLM 绕过，状态机在 Runtime 层，非 LLM 控制）。
//!
//! 状态转移（见 `docs/design.md` §3.6）：
//!
//! ```text
//! build_chat_request
//!    │
//!    ├─ token_count ≤ threshold  → 正常发送，record_success 重置计数
//!    └─ token_count > threshold  → 触发压缩管道
//!         │
//!         ├─ 压缩成功（token ≤ threshold）→ record_success，发送
//!         └─ 压缩失败 / 压缩后仍超阈值
//!              │
//!              ├─ fail_count < 3  → 注入警告，继续发送
//!              ├─ 3 ≤ fail_count < 5  → 熔断：注入错误中止本轮
//!              └─ fail_count ≥ 5  → 强制 TurnEnd
//! ```
//!
//! Thrash 检测：连续 `thrash_threshold` 次"压缩完即超阈值"→ 熔断。

/// 熔断器配置（阈值可配，见 `docs/design.md` §3.6）。
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreakerConfig {
    /// 压缩失败达到此阈值后熔断中止本轮（默认 3）。
    pub fail_threshold: usize,
    /// 压缩失败达到此阈值后强制 TurnEnd（默认 5）。
    pub force_end_threshold: usize,
    /// 连续"压缩完即超阈值"达到此阈值后判定 Thrash 并熔断（默认 2）。
    pub thrash_threshold: usize,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            fail_threshold: 3,
            force_end_threshold: 5,
            thrash_threshold: 2,
        }
    }
}

/// 压缩熔断状态机（C-29：状态机在 Runtime 层，非 LLM 控制）。
///
/// 跟踪两个独立计数器：
/// - `fail_count`：压缩管道返回错误（降级链全失败）的累计次数；
/// - `consecutive_oversize`：连续"压缩成功但 token 仍超阈值"的次数（Thrash 检测）。
///
/// `record_success` 重置两个计数器；`record_failure` 仅递增 `fail_count`；
/// `record_oversize` 仅递增 `consecutive_oversize`。
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    fail_count: usize,
    consecutive_oversize: usize,
    config: CircuitBreakerConfig,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker {
    /// 创建默认配置的熔断器（fail=3, `force_end=5`, thrash=2）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(CircuitBreakerConfig::default())
    }

    /// 创建指定配置的熔断器。
    #[must_use]
    pub fn with_config(config: CircuitBreakerConfig) -> Self {
        Self {
            fail_count: 0,
            consecutive_oversize: 0,
            config,
        }
    }

    /// 压缩成功（token 降到阈值下）：重置 `fail_count` 与 `consecutive_oversize`。
    pub fn record_success(&mut self) {
        self.fail_count = 0;
        self.consecutive_oversize = 0;
    }

    /// 压缩失败（降级链全失败）：`fail_count += 1`。
    pub fn record_failure(&mut self) {
        self.fail_count = self.fail_count.saturating_add(1);
    }

    /// 压缩成功但 token 仍超阈值（Thrash 前兆）：`consecutive_oversize += 1`。
    pub fn record_oversize(&mut self) {
        self.consecutive_oversize = self.consecutive_oversize.saturating_add(1);
    }

    /// 当前失败计数。
    #[must_use]
    pub fn fail_count(&self) -> usize {
        self.fail_count
    }

    /// 当前连续超阈值计数。
    #[must_use]
    pub fn consecutive_oversize(&self) -> usize {
        self.consecutive_oversize
    }

    /// 是否应熔断中止本轮（`fail_count >= fail_threshold`）。
    ///
    /// 满足后 `build_chat_request` 返回 `RuntimeError` 中止本轮（见 §3.6）。
    #[must_use]
    pub fn should_trip(&self) -> bool {
        self.fail_count >= self.config.fail_threshold
    }

    /// 是否应强制 `TurnEnd`（`fail_count >= force_end_threshold`）。
    ///
    /// 比熔断更严重：保留现场供 `/resume`（见 §3.6）。
    #[must_use]
    pub fn should_force_end(&self) -> bool {
        self.fail_count >= self.config.force_end_threshold
    }

    /// 是否检测到 Thrash（`consecutive_oversize >= thrash_threshold`）。
    ///
    /// 连续多次"压缩完即超阈值"说明压缩无法有效降低 token，继续压缩只会烧预算。
    /// 触发后熔断，同 `should_trip` 处理（见 §3.6）。
    #[must_use]
    pub fn is_thrashing(&self) -> bool {
        self.consecutive_oversize >= self.config.thrash_threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_clean() {
        let cb = CircuitBreaker::new();
        assert_eq!(cb.fail_count(), 0);
        assert_eq!(cb.consecutive_oversize(), 0);
        assert!(!cb.should_trip());
        assert!(!cb.should_force_end());
        assert!(!cb.is_thrashing());
    }

    #[test]
    fn record_success_resets_both_counters() {
        let mut cb = CircuitBreaker::new();
        cb.record_failure();
        cb.record_failure();
        cb.record_oversize();
        assert_eq!(cb.fail_count(), 2);
        assert_eq!(cb.consecutive_oversize(), 1);

        cb.record_success();
        assert_eq!(cb.fail_count(), 0);
        assert_eq!(cb.consecutive_oversize(), 0);
    }

    #[test]
    fn record_failure_increments_fail_count_only() {
        let mut cb = CircuitBreaker::new();
        cb.record_oversize(); // consecutive_oversize=1
        cb.record_failure();
        assert_eq!(cb.fail_count(), 1);
        // record_failure 不重置 consecutive_oversize（两者独立）
        assert_eq!(cb.consecutive_oversize(), 1);
    }

    #[test]
    fn record_oversize_increments_oversize_only() {
        let mut cb = CircuitBreaker::new();
        cb.record_failure(); // fail_count=1
        cb.record_oversize();
        assert_eq!(cb.consecutive_oversize(), 1);
        // record_oversize 不影响 fail_count
        assert_eq!(cb.fail_count(), 1);
    }

    #[test]
    fn should_trip_at_fail_threshold() {
        let mut cb = CircuitBreaker::new(); // fail_threshold=3
        cb.record_failure();
        cb.record_failure();
        assert!(!cb.should_trip()); // fail_count=2 < 3

        cb.record_failure();
        assert!(cb.should_trip()); // fail_count=3 >= 3
    }

    #[test]
    fn should_force_end_at_force_end_threshold() {
        let mut cb = CircuitBreaker::new(); // force_end_threshold=5
        for _ in 0..4 {
            cb.record_failure();
        }
        assert!(!cb.should_force_end()); // fail_count=4 < 5

        cb.record_failure();
        assert!(cb.should_force_end()); // fail_count=5 >= 5
        // force_end 蕴含 trip
        assert!(cb.should_trip());
    }

    #[test]
    fn is_thrashing_at_thrash_threshold() {
        let mut cb = CircuitBreaker::new(); // thrash_threshold=2
        cb.record_oversize();
        assert!(!cb.is_thrashing()); // consecutive_oversize=1 < 2

        cb.record_oversize();
        assert!(cb.is_thrashing()); // consecutive_oversize=2 >= 2
    }

    #[test]
    fn record_success_breaks_thrash_streak() {
        let mut cb = CircuitBreaker::new();
        cb.record_oversize();
        cb.record_oversize();
        assert!(cb.is_thrashing());

        // 一次成功压缩打破 Thrash 连续
        cb.record_success();
        assert!(!cb.is_thrashing());
        assert_eq!(cb.consecutive_oversize(), 0);
    }

    #[test]
    fn custom_config_thresholds() {
        let cb = CircuitBreaker::with_config(CircuitBreakerConfig {
            fail_threshold: 1,
            force_end_threshold: 2,
            thrash_threshold: 1,
        });
        let mut cb = cb;
        cb.record_failure();
        assert!(cb.should_trip()); // fail_threshold=1
        cb.record_failure();
        assert!(cb.should_force_end()); // force_end_threshold=2

        let mut cb2 = CircuitBreaker::with_config(CircuitBreakerConfig {
            fail_threshold: 10,
            force_end_threshold: 20,
            thrash_threshold: 1,
        });
        cb2.record_oversize();
        assert!(cb2.is_thrashing()); // thrash_threshold=1
    }

    #[test]
    fn counters_saturate_without_overflow() {
        let mut cb = CircuitBreaker::new();
        for _ in 0..100 {
            cb.record_failure();
            cb.record_oversize();
        }
        // 不 panic，计数器饱和（实际 usize 不会溢出，saturating_add 保护）
        assert!(cb.should_trip());
        assert!(cb.should_force_end());
        assert!(cb.is_thrashing());
    }
}
