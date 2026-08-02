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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::TokenBudget;
    use minicoding_core::model::{
        ContentBlock, LlmError, Message, MessageMeta, Role, StopReason, ToolCallId, ToolContent,
    };
    use minicoding_core::provider::{
        BoxFuture, BoxStream, Capabilities, ChatRequest, Delta, LlmProvider, Tokenizer,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 按字符数计数的分词器（1 字符 = 1 token，含 `tool_result` 文本内容）。
    struct CharTokenizer;

    impl Tokenizer for CharTokenizer {
        fn count(&self, text: &str) -> usize {
            text.chars().count()
        }
        fn count_messages(&self, msgs: &[Message]) -> usize {
            msgs.iter()
                .map(|m| {
                    let mut total = 0;
                    for block in &m.content {
                        match block {
                            ContentBlock::Text { text } => total += text.chars().count(),
                            ContentBlock::ToolResult { content, .. } => {
                                if let ToolContent::Text(t) = content {
                                    total += t.chars().count();
                                }
                            }
                            ContentBlock::Image { .. } | ContentBlock::ToolUse(_) => {}
                        }
                    }
                    total
                })
                .sum()
        }
        fn id(&self) -> &'static str {
            "char-test"
        }
    }

    /// mock `LlmProvider`：`chat_stream` 返回固定摘要文本，记录调用次数。
    struct MockSummaryProvider {
        call_count: AtomicUsize,
    }

    impl MockSummaryProvider {
        fn new() -> Self {
            Self {
                call_count: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    impl LlmProvider for MockSummaryProvider {
        fn id(&self) -> &'static str {
            "mock-summary"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_tool_call: false,
                supports_vision: false,
                supports_streaming: true,
                supports_json_mode: false,
                context_window: 4096,
                max_output: 1024,
            }
        }
        fn tokenizer(&self) -> Arc<dyn Tokenizer> {
            Arc::new(CharTokenizer)
        }
        fn chat_stream(
            &self,
            _req: ChatRequest,
        ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let stream = futures::stream::iter(vec![
                    Ok(Delta::Text("short summary".to_string())),
                    Ok(Delta::Stop(StopReason::EndTurn)),
                ]);
                Ok(Box::pin(stream) as BoxStream<'static, _>)
            })
        }
        fn count_tokens(&self, messages: &[Message]) -> BoxFuture<'_, usize> {
            let n = messages.len();
            Box::pin(async move { n })
        }
    }

    /// 构造 `tool_result` 消息（用于 L1 裁剪测试）。
    fn make_tool_result_msg(text: &str) -> Message {
        Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                call_id: String::new() as ToolCallId,
                content: ToolContent::Text(text.to_string()),
                is_error: false,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: time::OffsetDateTime::now_utc(),
            metadata: MessageMeta::default(),
        }
    }

    // === 场景 1：消息 token 未超阈值时，pipeline 不修改消息 ===

    #[tokio::test]
    async fn under_threshold_no_modification() {
        let tokenizer = CharTokenizer;
        let budget = TokenBudget::new(10_000); // threshold = (10000-4096-1024)*0.85 = 4148
        let mut msgs = vec![Message::user_text("hello"), Message::user_text("world")];
        // 10 tokens < 4148，不应触发任何压缩级别
        let result = compress_pipeline(&mut msgs, &tokenizer, &budget, None)
            .await
            .expect("compress_pipeline 应成功");
        assert_eq!(result.clipped_count, 0);
        assert_eq!(result.summarized_count, 0);
        assert_eq!(result.dropped_count, 0);
        assert_eq!(result.truncated_count, 0);
        assert_eq!(msgs.len(), 2, "消息不应被修改");
    }

    // === 场景 2：消息超阈值时，L1 clip 裁剪工具结果 ===

    #[tokio::test]
    async fn l1_clips_large_tool_result() {
        let tokenizer = CharTokenizer;
        // 用大窗口让 threshold 足够高，使 L1 裁剪后不再触发 L3/L4
        // threshold = (10000 - 100) * 0.85 = 8415
        let budget = TokenBudget {
            context_window: 10_000,
            reserved_output: 100,
            safety_margin: 0,
        };
        // 9000 字符 > 8415 threshold，触发压缩
        let big = "x".repeat(9_000);
        let mut msgs = vec![make_tool_result_msg(&big)];
        let tokens_before = tokenizer.count_messages(&msgs);
        assert!(tokens_before > budget.compact_threshold());

        let result = compress_pipeline(&mut msgs, &tokenizer, &budget, None)
            .await
            .expect("compress_pipeline 应成功");
        // L1 应裁剪该 tool_result（9000 > 2000 阈值字符）
        assert!(result.clipped_count > 0, "L1 应裁剪大 tool_result");
        // 裁剪后应降至阈值下，L3/L4 不触发
        assert_eq!(result.dropped_count, 0, "L3 不应触发");
        assert_eq!(result.truncated_count, 0, "L4 不应触发");
        assert!(
            tokenizer.count_messages(&msgs) <= budget.compact_threshold(),
            "裁剪后应低于阈值"
        );
    }

    // === 场景 3：L1 不足时降级 L3 rolling（无 provider 跳过 L2）===

    #[tokio::test]
    async fn l3_rolling_when_l1_insufficient_no_provider() {
        let tokenizer = CharTokenizer;
        // threshold = (6000 - 100) * 0.85 = 5015
        let budget = TokenBudget {
            context_window: 6_000,
            reserved_output: 100,
            safety_margin: 0,
        };
        // 30 条 × 200 字符 = 6000 tokens > 5015
        let mut msgs: Vec<Message> = (0..30)
            .map(|_| Message::user_text("x".repeat(200)))
            .collect();
        let tokens_before = tokenizer.count_messages(&msgs);
        assert!(tokens_before > budget.compact_threshold());

        let result = compress_pipeline(&mut msgs, &tokenizer, &budget, None)
            .await
            .expect("compress_pipeline 应成功");
        // L1：无 tool_result，不裁剪
        assert_eq!(result.clipped_count, 0, "L1 不应裁剪纯文本消息");
        // L2：无 provider，跳过
        assert_eq!(result.summarized_count, 0, "无 provider 时 L2 应跳过");
        // L3：30 > 20，丢弃 10 条
        assert_eq!(result.dropped_count, 10, "L3 rolling 应丢弃 10 条最旧消息");
        // L3 后 20 × 200 = 4000 < 5015，L4 不触发
        assert_eq!(result.truncated_count, 0, "L4 不应触发");
        assert_eq!(msgs.len(), 20, "应保留 20 条");
    }

    // === 场景 4：L3 不足时降级 L4 hard_truncate ===

    #[tokio::test]
    async fn l4_hard_truncate_when_l3_insufficient() {
        let tokenizer = CharTokenizer;
        // 极小窗口让 threshold 极低，L3 保留 20 条后仍超阈值
        // threshold = 200 * 0.85 = 170
        let budget = TokenBudget {
            context_window: 200,
            reserved_output: 0,
            safety_margin: 0,
        };
        // 30 条 × 10 字符 = 300 > 170
        // L3: keep 20 → 200 > 170，仍超阈值
        // L4: 从头部丢弃直到 ≤ 170
        let mut msgs: Vec<Message> = (0..30)
            .map(|_| Message::user_text("0123456789")) // 10 字符
            .collect();
        let tokens_before = tokenizer.count_messages(&msgs);
        assert!(tokens_before > budget.compact_threshold());

        let result = compress_pipeline(&mut msgs, &tokenizer, &budget, None)
            .await
            .expect("compress_pipeline 应成功");
        // L3 丢弃 10 条
        assert_eq!(result.dropped_count, 10, "L3 应丢弃 10 条");
        // L4 必须触发（L3 后 200 > 170）
        assert!(
            result.truncated_count > 0,
            "L4 应丢弃额外消息: truncated_count={}",
            result.truncated_count
        );
        assert!(
            tokenizer.count_messages(&msgs) <= budget.compact_threshold(),
            "L4 后应降至阈值下: {}",
            tokenizer.count_messages(&msgs)
        );
    }

    // === 场景 5：有 provider 时 L2 summarize 被调用 ===

    #[tokio::test]
    async fn l2_summarize_with_provider() {
        let tokenizer = CharTokenizer;
        // threshold = (6000 - 100) * 0.85 = 5015
        let budget = TokenBudget {
            context_window: 6_000,
            reserved_output: 100,
            safety_margin: 0,
        };
        // 10 条 × 600 字符 = 6000 > 5015
        let mut msgs: Vec<Message> = (0..10)
            .map(|_| Message::user_text("x".repeat(600)))
            .collect();
        let tokens_before = tokenizer.count_messages(&msgs);
        assert!(tokens_before > budget.compact_threshold());

        let provider = MockSummaryProvider::new();
        let result = compress_pipeline(&mut msgs, &tokenizer, &budget, Some(&provider))
            .await
            .expect("compress_pipeline 应成功");
        // L2 应摘要 5 条（ratio=0.5，10 条 × 0.5 = 5）
        assert!(
            result.summarized_count > 0,
            "L2 应摘要消息: summarized_count={}",
            result.summarized_count
        );
        assert_eq!(result.summarized_count, 5, "L2 应摘要 5 条（10 × 0.5）");
        assert!(
            provider.call_count() > 0,
            "provider 应被调用: call_count={}",
            provider.call_count()
        );
        // 10 - 5 + 1(摘要消息) = 6 条
        assert_eq!(msgs.len(), 6, "摘要后应剩 6 条（5 原文 + 1 摘要）");
        // 摘要后 token 应降至阈值下
        assert!(
            tokenizer.count_messages(&msgs) <= budget.compact_threshold(),
            "L2 后应低于阈值: {}",
            tokenizer.count_messages(&msgs)
        );
    }
}
