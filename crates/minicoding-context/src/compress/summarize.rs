//! L2 旧消息摘要（见 `docs/design.md` §3.3）。
//!
//! 对权重最低的 N 条非 system 消息调 LLM 生成摘要，摘要替换原文并标注
//! `[summarized @ ts]`。LLM 调用失败时走降级链（§3.8，见 `fallback.rs`），
//! 启发式兜底恒成功，故本函数不因 LLM 失败而返回错误。

use minicoding_core::model::{
    CompressedRange, ContentBlock, Message, MessageMeta, MessageSource, Role, RuntimeError,
};
use minicoding_core::provider::{LlmProvider, Tokenizer};
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
    /// 单次 LLM 摘要调用超时秒数（默认 30，对齐 `memory/session_sum.rs`）。
    ///
    /// CT-1（2026-08-25 审查）：此前无超时——provider 挂起时 `ContextManagerImpl`
    /// 持有的 messages 写锁被无限期占用，阻塞所有 `append`/`build_chat_request`。
    /// 超时按该 provider 失败处理，进入降级链（C-29 不可跳过）。
    pub llm_timeout_secs: u64,
}

impl Default for SummarizeConfig {
    fn default() -> Self {
        Self {
            ratio: 0.5,
            max_summary_tokens: 200,
            llm_timeout_secs: 30,
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
/// CT-2（2026-08-25 审查）：选取集经配对组扩展——`assistant(tool_calls)` 与其后
/// 紧随的 `Role::Tool` 结果原子替换，实际替换数可能大于 N。
///
/// system 消息权重恒 ≥ 1.0（见 `weight.rs`），不会被选中。
///
/// # Errors
/// 降级链（§3.8）保证 LLM 失败时不抛错。仅当降级链终端也失败时返回错误
/// （理论不可达，启发式兜底恒成功）。
pub async fn summarize_old_messages(
    messages: &mut Vec<Message>,
    tokenizer: &dyn Tokenizer,
    provider: &dyn LlmProvider,
    config: &SummarizeConfig,
    result: &mut CompressResult,
    anchor_seq: Option<u64>,
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

    // CT-2（2026-08-25 审查）：配对组扩展——选中组内任一成员则整组纳入替换，
    // 避免"摘要掉 tool_call、留下孤儿 tool_result"（或反向）破坏后续请求；
    // 替换产物是纯文本 assistant 消息，不含任何工具引用。扩展后索引仍升序去重。
    let to_summarize = super::tool_group::expand_to_groups(messages, &to_summarize);

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

    // M-07（R-02）：推算被替代消息的序号区间与掉 token 量
    // （消息序号锚点：压缩前最后消息序号 = anchor_seq，index i 的序号 =
    // anchor_seq - (压缩前消息数 - 1 - i)）。
    let compressed_range = anchor_seq.map(|anchor| {
        let total = messages.len();
        let from_index = to_summarize[0];
        let to_index = to_summarize[to_summarize.len() - 1];
        let seq_of = |i: usize| anchor - (total as u64 - 1 - i as u64);
        let from_seq = seq_of(from_index);
        let to_seq = seq_of(to_index);
        let dropped_tokens: usize = selected
            .iter()
            .map(|m| tokenizer.count_messages(std::slice::from_ref(m)))
            .sum();
        result.dropped_tokens += dropped_tokens;
        CompressedRange {
            from_seq,
            to_seq,
            dropped_tokens,
        }
    });

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
            compressed_range,
            ..Default::default()
        },
    };

    // 从后向前删除被摘要消息（索引升序，逆序 remove 不偏移），再在原首位置插入摘要
    for &i in to_summarize.iter().rev() {
        messages.remove(i);
    }
    messages.insert(insert_pos, summary_msg);

    // CT-2：实际替换数 = 组扩展后的集合大小（可能大于权重选取的 n）
    result.summarized_count += to_summarize.len();
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 最小单元测试：CT-2 配对组原子替换（2026-08-25 审查）。

    use super::*;
    use minicoding_core::model::{LlmError, StopReason, ToolCall};
    use minicoding_core::provider::{BoxFuture, BoxStream, Capabilities, ChatRequest, Delta};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 按字符数计数的分词器。
    struct CharTokenizer;
    impl Tokenizer for CharTokenizer {
        fn count(&self, text: &str) -> usize {
            text.chars().count()
        }
        fn count_messages(&self, msgs: &[Message]) -> usize {
            msgs.iter().map(|m| m.text().chars().count()).sum()
        }
        fn id(&self) -> &'static str {
            "char-test"
        }
    }

    /// mock provider：恒返回固定摘要文本。
    struct MockSummaryProvider {
        call_count: AtomicUsize,
    }
    impl MockSummaryProvider {
        fn new() -> Self {
            Self {
                call_count: AtomicUsize::new(0),
            }
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
                    Ok(Delta::Text("summary".to_string())),
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

    /// 构造带单个 `tool_call` 的 assistant 消息（组头）。
    fn assistant_with_call(call_id: &str) -> Message {
        Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "calling".into(),
            }],
            tool_calls: vec![ToolCall {
                id: call_id.to_string(),
                name: "fs.read".into(),
                input: serde_json::json!({"path": "a.rs"}),
            }],
            tool_call_id: None,
            created_at: OffsetDateTime::now_utc(),
            metadata: MessageMeta::default(),
        }
    }

    /// 构造 `Role::Tool` 结果消息（组成员）。
    fn tool_result(call_id: &str) -> Message {
        Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                call_id: call_id.to_string(),
                content: minicoding_core::model::ToolContent::Text("done".into()),
                is_error: false,
                metadata: minicoding_core::model::ToolResultMeta::default(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.to_string()),
            created_at: OffsetDateTime::now_utc(),
            metadata: MessageMeta::default(),
        }
    }

    #[tokio::test]
    async fn summarize_expands_tool_group_atomically() {
        // 权重选取会命中低 base 的 tool 结果（0.4），而其 assistant 组头未必入选；
        // 扩展后整组替换为单条摘要消息，不残留孤儿。
        // [0]=user 长(拉低权重) [1]=A(tc) [2]=T [3]=user 短
        let mut msgs = vec![
            Message::user_text("x".repeat(600)),
            assistant_with_call("c1"),
            tool_result("c1"),
            Message::user_text("recent question"),
        ];
        let tokenizer = CharTokenizer;
        let provider = MockSummaryProvider::new();
        let mut result = CompressResult::default();

        summarize_old_messages(
            &mut msgs,
            &tokenizer,
            &provider,
            &SummarizeConfig::default(),
            &mut result,
            Some(4),
        )
        .await
        .expect("summarize 应成功");

        assert!(
            !msgs.iter().any(|m| m.role == Role::Tool),
            "不允许残留孤儿 tool_result"
        );
        assert_eq!(result.summarized_count, 3, "user + A + T 整组替换");
        // 摘要消息插入原首位置，其后是未选中的新消息
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].metadata.summarized, "首条应为摘要消息");
        assert_eq!(msgs[1].text(), "recent question");
    }
}
