//! L2 旧消息摘要（见 `docs/design.md` §3.3）。
//!
//! 对权重最低的 N 条非 system 消息调 LLM 生成摘要，摘要替换原文并标注
//! `[summarized @ ts]`。LLM 调用失败时走降级链（§3.8，见 `fallback.rs`），
//! 启发式兜底恒成功，故本函数不因 LLM 失败而返回错误。

use minicoding_core::model::{
    ContentBlock, Message, MessageMeta, MessageSource, Role, RuntimeError,
};
use minicoding_core::provider::LlmProvider;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::weight::message_weight;

use super::CompressResult;
use super::fallback::summarize_with_fallback;

/// L2 摘要配置。
#[derive(Debug, Clone)]
pub struct SummarizeConfig {
    /// 待摘要消息占非 system 消息的比例（默认 0.5，即半数）。
    pub ratio: f64,
    /// 摘要最大 token 数（默认 200）。
    pub max_summary_tokens: usize,
}

impl Default for SummarizeConfig {
    fn default() -> Self {
        Self {
            ratio: 0.5,
            max_summary_tokens: 200,
        }
    }
}

/// L2 旧消息摘要。
///
/// 选取权重最低的 N 条非 system 消息（`N = 非system消息数 × ratio`，向下取整），
/// 调 `summarize_with_fallback`（含降级链）生成摘要，替换为单条标注
/// `[summarized @ ts]` 的 assistant 消息（`metadata.summarized = true`）。
/// 替换数记入 `result.summarized_count`；降级到启发式兜底时记 `result.fallback_used`。
///
/// system 消息权重恒 ≥ 1.0（见 `weight.rs`），不会被选中。
///
/// # Errors
/// 降级链（§3.8）保证 LLM 失败时不抛错。仅当降级链终端也失败时返回错误
/// （理论不可达，启发式兜底恒成功）。
pub async fn summarize_old_messages(
    messages: &mut Vec<Message>,
    provider: &dyn LlmProvider,
    config: &SummarizeConfig,
    result: &mut CompressResult,
) -> Result<(), RuntimeError> {
    let total = messages.len();
    if total < 2 {
        return Ok(());
    }

    // 计算非 system 消息的权重，选最低的 N 条
    let mut weighted: Vec<(usize, f64)> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role != Role::System)
        .map(|(i, m)| (i, message_weight(m, i, total)))
        .collect();

    // 消息数远小于 f64 尾数精度，且 ratio ∈ [0,1] 结果恒非负；按 design.md §3.3 取半数。
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let n = (weighted.len() as f64 * config.ratio) as usize;
    if n == 0 {
        return Ok(());
    }

    // 按权重升序排序，取最低的 N 个，再按原位置排序以保持上下文顺序
    weighted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut to_summarize: Vec<usize> = weighted.iter().take(n).map(|(i, _)| *i).collect();
    to_summarize.sort_unstable();

    // 收集待摘要消息的引用（供降级链渲染与启发式兜底使用）
    let selected: Vec<&Message> = to_summarize
        .iter()
        .filter_map(|&i| messages.get(i))
        .collect();

    // 调降级链生成摘要（主 provider → 备用 → 启发式兜底，C-29 不可跳过）
    let summary = summarize_with_fallback(&selected, provider, None, config).await?;

    // 启发式兜底产生的摘要以 `[heuristic fallback]` 开头，标记降级发生
    if summary.starts_with("[heuristic fallback]") {
        result.fallback_used = true;
    }

    // 替换：在第一个被摘要消息的位置插入摘要消息，删除所有被摘要消息
    let insert_pos = to_summarize[0];
    let now = OffsetDateTime::now_utc();
    let ts = now.format(&Rfc3339).unwrap_or_default();
    let summary_msg = Message {
        id: ulid::Ulid::new().to_string(),
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: format!("[summarized @ {ts}]\n{summary}"),
        }],
        tool_calls: Vec::new(),
        tool_call_id: None,
        created_at: now,
        metadata: MessageMeta {
            summarized: true,
            source: MessageSource::Llm,
            ..Default::default()
        },
    };

    // 从后向前删除被摘要消息（避免索引偏移），再在原首位置插入摘要
    for &i in to_summarize.iter().rev() {
        messages.remove(i);
    }
    messages.insert(insert_pos, summary_msg);

    result.summarized_count += n;
    Ok(())
}
