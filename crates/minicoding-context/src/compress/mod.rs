//! 4 级压缩管道编排（见 `docs/design.md` §3.3）。
//!
//! 当 `token_count > budget.compact_threshold()`（usable × 0.85）时触发压缩管道，
//! 逐级尝试：
//!
//! - **L1 工具结果裁剪**：大于阈值的 `tool_result` 截断为 "前 K 行 + ... + 后 K 行 + 元信息"
//! - **L2 旧消息摘要**：对权重最低的 N 条消息调 LLM 生成摘要，替换原文
//! - **L3 滚动窗口**：仅保留最近 W 条非 system 消息 + 全部 system 消息
//! - **L4 硬截断**：兜底，按 token 数从尾部保留，记录 warn 日志
//!
//! 每级后检查 token 是否降到阈值以下，降了则提前返回（C-29：降级链顺序不可跳）。
//! L2 需 `LlmProvider`，为 `None` 时跳过 L2（其余级别仍按序执行）。

use minicoding_core::model::{Message, RuntimeError};
use minicoding_core::provider::{LlmProvider, Tokenizer};
use tracing::Instrument;

use crate::budget::TokenBudget;

pub mod circuit_breaker;
pub mod clip;
pub mod fallback;
pub mod hard_truncate;
pub mod rolling;
pub mod state_keep;
pub mod summarize;

pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
pub use clip::ClipConfig;
pub use fallback::summarize_with_fallback;
pub use hard_truncate::hard_truncate;
pub use rolling::{RollingConfig, rolling_window};
pub use state_keep::StateKeep;
pub use summarize::{SummarizeConfig, summarize_old_messages};

/// 压缩结果统计（记录每级压缩的执行情况）。
#[derive(Debug, Clone, Default)]
pub struct CompressResult {
    /// L1 裁剪的 `tool_result` 块数。
    pub clipped_count: usize,
    /// L2 摘要替换的消息数。
    pub summarized_count: usize,
    /// L3 滚动窗口丢弃的消息数。
    pub dropped_count: usize,
    /// L4 硬截断丢弃的消息数。
    pub truncated_count: usize,
    /// L2 是否降级到启发式兜底（C-29 降级链，见 `fallback.rs`）。
    pub fallback_used: bool,
}

/// 计算消息序列的 token 数。
fn token_count(messages: &[Message], tokenizer: &dyn Tokenizer) -> usize {
    tokenizer.count_messages(messages)
}

/// 4 级压缩管道入口。
///
/// 按 `docs/design.md` §3.3 顺序执行 L1→L2→L3→L4，每级后检查 token 是否降到
/// `budget.compact_threshold()` 以下，降了则提前返回。L2 需要 `provider`，
/// 为 `None` 时跳过 L2（L1→L3→L4 仍按序执行）。
///
/// # Errors
/// L2 摘要走降级链（§3.8），启发式兜底恒成功，故 LLM 失败不传播。仅当降级链
/// 终端也失败时返回 `RuntimeError`（理论不可达）。
pub async fn compress_pipeline(
    messages: &mut Vec<Message>,
    tokenizer: &dyn Tokenizer,
    budget: &TokenBudget,
    provider: Option<&dyn LlmProvider>,
) -> Result<CompressResult, RuntimeError> {
    let mut result = CompressResult::default();
    let threshold = budget.compact_threshold();

    // L1: 工具结果裁剪（同步）
    {
        let _span = tracing::info_span!("compress", level = "L1").entered();
        clip::clip_tool_results(messages, &ClipConfig::default(), &mut result);
    }
    if token_count(messages, tokenizer) <= threshold {
        return Ok(result);
    }

    // L2: 旧消息摘要（需 provider，异步调 LLM）
    if let Some(p) = provider {
        summarize_old_messages(messages, p, &SummarizeConfig::default(), &mut result)
            .instrument(tracing::info_span!("compress", level = "L2"))
            .await?;
        if token_count(messages, tokenizer) <= threshold {
            return Ok(result);
        }
    }

    // L3: 滚动窗口（同步）
    {
        let _span = tracing::info_span!("compress", level = "L3").entered();
        rolling_window(messages, &RollingConfig::default(), &mut result);
    }
    if token_count(messages, tokenizer) <= threshold {
        return Ok(result);
    }

    // L4: 硬截断兜底（同步）
    {
        let _span = tracing::info_span!("compress", level = "L4").entered();
        hard_truncate(messages, tokenizer, budget, &mut result);
    }
    Ok(result)
}
