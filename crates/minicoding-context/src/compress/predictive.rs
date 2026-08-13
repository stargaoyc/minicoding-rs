//! C-08：预测性压缩（见 `docs/design.md` §3.9）。
//!
//! 与反应式 compact 互补：根据历史 turn token 增长估算下一 turn 是否会超出
//! 上下文窗口，在超出前提前 compact，避免 LLM 调用因 token 超限而失败。
//!
//! ## 算法
//!
//! 1. `PredictiveTracker` 记录每 turn 结束时的 token 总量，维护滑动窗口（最近 N turn）
//! 2. `should_predict_compact` 计算平均每 turn 增长量，预估下一 turn 的 token 总量
//! 3. 若预估超出 `compact_threshold`，返回 `true` 触发提前压缩
//!
//! 历史数据不足（< 2 turn）时使用 `predictive_baseline_growth_tokens` 作为基线估算。

use std::collections::VecDeque;

/// 滑动窗口大小（保留最近 N turn 的 token 记录）。
const WINDOW_SIZE: usize = 10;

/// 预测性压缩追踪器（C-08）。
///
/// 记录每 turn 结束时的 token 总量，用于估算下一 turn 的增长趋势。
/// 线程安全：由 `ContextManagerImpl` 通过 `Mutex` 保护。
#[derive(Debug, Clone)]
pub struct PredictiveTracker {
    /// 最近 N turn 的 token 总量快照（按时间顺序，最新在尾部）。
    history: VecDeque<usize>,
}

impl PredictiveTracker {
    /// 创建空追踪器。
    #[must_use]
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(WINDOW_SIZE),
        }
    }

    /// 记录一个 turn 结束时的 token 总量。
    pub fn record_turn(&mut self, token_count: usize) {
        if self.history.len() >= WINDOW_SIZE {
            self.history.pop_front();
        }
        self.history.push_back(token_count);
    }

    /// 计算历史平均每 turn token 增长量。
    ///
    /// 返回 `None` 表示历史数据不足（< 2 turn）。
    fn avg_growth(&self) -> Option<usize> {
        if self.history.len() < 2 {
            return None;
        }
        let vec: Vec<&usize> = self.history.iter().collect();
        let diffs: Vec<usize> = vec
            .windows(2)
            .map(|w| (*w[1]).saturating_sub(*w[0]))
            .collect();
        let total: usize = diffs.iter().sum();
        // 向上取整避免低估
        Some(total.div_ceil(diffs.len()))
    }

    /// 获取历史记录数。
    ///
    /// 当前内部逻辑仅写入历史，不读取长度；保留供测试与下游诊断使用。
    #[must_use]
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// 是否为空。
    ///
    /// 当前内部逻辑不读取；保留供测试与下游诊断使用（与 `len` 配对）。
    #[must_use]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

impl Default for PredictiveTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// 判断是否应触发预测性压缩（C-08）。
///
/// 根据历史 token 增长趋势估算下一 turn 的 token 总量，若预估超出
/// `compact_threshold` 则返回 `true`。
///
/// # 参数
/// - `current_tokens`：当前 token 总量
/// - `compact_threshold`：压缩阈值（`budget.compact_threshold()`）
/// - `tracker`：预测性压缩追踪器
/// - `baseline_growth`：历史不足时的基线增长量（`predictive_baseline_growth_tokens`）
///
/// # 返回
/// `true` 表示应提前压缩，`false` 表示无需。
#[must_use]
pub fn should_predict_compact(
    current_tokens: usize,
    compact_threshold: usize,
    tracker: &PredictiveTracker,
    baseline_growth: usize,
) -> bool {
    let growth = tracker.avg_growth().unwrap_or(baseline_growth);
    let predicted = current_tokens.saturating_add(growth);
    predicted > compact_threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_is_empty() {
        let t = PredictiveTracker::new();
        assert!(t.is_empty(), "expected empty: t");
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn record_turn_grows_history() {
        let mut t = PredictiveTracker::new();
        t.record_turn(100);
        t.record_turn(200);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn history_caps_at_window_size() {
        let mut t = PredictiveTracker::new();
        for i in 0..(WINDOW_SIZE + 5) {
            t.record_turn(i * 100);
        }
        assert_eq!(t.len(), WINDOW_SIZE);
    }

    #[test]
    fn avg_growth_with_insufficient_data_returns_none() {
        let mut t = PredictiveTracker::new();
        t.record_turn(100);
        assert!(t.avg_growth().is_none());
    }

    #[test]
    fn avg_growth_calculates_average_difference() {
        let mut t = PredictiveTracker::new();
        t.record_turn(100);
        t.record_turn(150);
        t.record_turn(250);
        // diffs: 50, 100 → avg = 75
        assert_eq!(t.avg_growth(), Some(75));
    }

    #[test]
    fn should_predict_returns_false_when_well_under_threshold() {
        let mut tracker = PredictiveTracker::new();
        tracker.record_turn(100);
        tracker.record_turn(200);
        // current=200, growth=100, predicted=300 < 1000
        assert!(!should_predict_compact(200, 1000, &tracker, 15000));
    }

    #[test]
    fn should_predict_returns_true_when_predicted_exceeds_threshold() {
        let mut tracker = PredictiveTracker::new();
        tracker.record_turn(800);
        tracker.record_turn(900);
        // current=900, growth=100, predicted=1000 == 1000, NOT > 1000
        assert!(!should_predict_compact(900, 1000, &tracker, 15000));
        // current=950, growth=100, predicted=1050 > 1000
        assert!(should_predict_compact(950, 1000, &tracker, 15000));
    }

    #[test]
    fn should_predict_uses_baseline_when_history_insufficient() {
        let tracker = PredictiveTracker::new();
        // 无历史 → baseline=500, current=600, predicted=1100 > 1000
        assert!(should_predict_compact(600, 1000, &tracker, 500));
        // baseline=100, current=800, predicted=900 < 1000
        assert!(!should_predict_compact(800, 1000, &tracker, 100));
    }

    #[test]
    fn avg_growth_with_decreasing_tokens() {
        let mut t = PredictiveTracker::new();
        t.record_turn(500);
        t.record_turn(300);
        t.record_turn(200);
        // diffs: 0 (saturating), 0 → avg = 0
        assert_eq!(t.avg_growth(), Some(0));
    }
}
