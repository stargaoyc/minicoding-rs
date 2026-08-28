//! Token 预算计算（见 `docs/design.md` §3.4）。
//!
//! 预算分配：`budget_total = model.context_window`，
//! `budget_reserved = output_tokens (默认 4096) + safety_margin (1024)`，
//! `budget_usable = budget_total - budget_reserved`。
//!
//! 压缩触发阈值 = `budget_usable * 0.85`（见 `docs/design.md` §3.3）。

/// Token 预算配置。
///
/// 描述模型上下文窗口的预算分配：预留输出、安全余量、可用预算与压缩阈值。
#[derive(Debug, Clone, Copy)]
pub struct TokenBudget {
    /// 模型上下文窗口（如 128000）。
    pub context_window: usize,
    /// 预留输出 token（默认 4096）。
    pub reserved_output: usize,
    /// 安全余量（默认 1024），防止计数误差越界。
    pub safety_margin: usize,
    /// 压缩触发比例（CTX-R6-7，2026-08-28 R6 审查：此前硬编码 0.85，
    /// `config.budget_ratio` 字段零消费——改为可配置，默认 0.85 保持原行为）。
    pub ratio: f64,
}

impl TokenBudget {
    /// 创建指定上下文窗口的预算，预留输出 4096、安全余量 1024、触发比例 0.85。
    #[must_use]
    pub fn new(context_window: usize) -> Self {
        Self {
            context_window,
            reserved_output: 4096,
            safety_margin: 1024,
            ratio: 0.85,
        }
    }

    /// 设置压缩触发比例（builder，CTX-R6-7：由 `config.budget_ratio` 驱动）。
    #[must_use]
    pub fn with_ratio(mut self, ratio: f64) -> Self {
        self.ratio = ratio.clamp(0.1, 1.0);
        self
    }

    /// 可用预算 = 窗口 − 预留输出 − 安全余量。
    ///
    /// 用 `saturating_sub` 避免窗口过小时下溢 panic（见 AGENTS.md §2.3 不 panic）。
    #[must_use]
    pub fn usable(&self) -> usize {
        self.context_window
            .saturating_sub(self.reserved_output)
            .saturating_sub(self.safety_margin)
    }

    /// 压缩触发阈值 = 可用预算 × `ratio`（默认 0.85，见 `docs/design.md` §3.3）。
    ///
    /// 当 `token_count` 超过此阈值时触发压缩管道。
    #[must_use]
    // 上下文窗口远小于 f64 尾数精度，且结果恒非负；按 design.md §3.4 用比例系数。
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn compact_threshold(&self) -> usize {
        (self.usable() as f64 * self.ratio) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_defaults() {
        let b = TokenBudget::new(128_000);
        assert_eq!(b.context_window, 128_000);
        assert_eq!(b.reserved_output, 4096);
        assert_eq!(b.safety_margin, 1024);
    }

    #[test]
    fn usable_subtracts_reserved_and_margin() {
        let b = TokenBudget::new(128_000);
        assert_eq!(b.usable(), 128_000 - 4096 - 1024);
    }

    #[test]
    fn usable_saturates_on_tiny_window() {
        // 窗口小于预留+余量时不应 panic，而是 saturating 到 0。
        let b = TokenBudget::new(1024);
        assert_eq!(b.usable(), 0);
    }

    #[test]
    // 测试镜像实现公式，usize↔f64 转换与生产代码同源。
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn compact_threshold_is_85_percent_of_usable() {
        let b = TokenBudget::new(128_000);
        let usable = b.usable();
        assert_eq!(b.compact_threshold(), (usable as f64 * 0.85) as usize);
        // 128000 - 4096 - 1024 = 122880; * 0.85 = 104448
        assert_eq!(b.compact_threshold(), 104_448);
    }
}
