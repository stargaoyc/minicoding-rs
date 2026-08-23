//! L2 摘要失败降级链（见 `docs/design.md` §3.8）。
//!
//! L2"旧消息摘要"需调 LLM 生成摘要，可能失败（限流/超时/过滤）。失败时按降级链
//! 处理，**永不**向上抛错中断对话（C-29：降级链不可跳过）：
//!
//! ```text
//! L2 主 provider → 失败
//!   └─ L2 备用 provider（如有）→ 失败
//!        └─ 启发式兜底（取每条消息首 200 字符拼接为摘要）→ 必成功
//!             └─ 跳过 L2，直接进 L3（仅当启发式也失败，理论不可达）
//! ```
//!
//! 降级时记 `tracing::warn!` 日志。启发式兜底标注 `[heuristic fallback]`。

use futures::StreamExt;
use minicoding_core::model::{Message, RuntimeError};
use minicoding_core::provider::{ChatRequest, Delta, GenerationParams, LlmProvider};

use super::summarize::SummarizeConfig;

/// 启发式兜底取每条消息首 N 字符（见 `docs/design.md` §3.8 任务规格）。
const HEURISTIC_CHARS_PER_MSG: usize = 200;

/// 带降级链的摘要生成（C-29：L2 失败必走降级链）。
///
/// 按序尝试：主 provider → 备用 provider（如有）→ 启发式兜底。启发式兜底取每条
/// 消息首 200 字符用 `; ` 拼接，标注 `[heuristic fallback]`，**必成功**。
///
/// # Errors
/// 启发式兜底恒成功，故实际不返回 `Err`。保留 `Result` 类型以与管道 `?` 兼容，
/// 并为未来"跳过 L2 直接进 L3"的降级路径预留（见 §3.8 第 4 步）。
pub async fn summarize_with_fallback(
    messages: &[&Message],
    primary: &dyn LlmProvider,
    secondary: Option<&dyn LlmProvider>,
    config: &SummarizeConfig,
) -> Result<String, RuntimeError> {
    // 渲染待摘要消息为 LLM 输入文本
    let input = messages
        .iter()
        .map(|m| format!("[{}] {}", role_label(&m.role), m.full_text()))
        .collect::<Vec<_>>()
        .join("\n\n");

    // 1. 主 provider
    match call_llm_summary(primary, &input, config).await {
        Ok(summary) => return Ok(summary),
        Err(e) => {
            tracing::warn!(error = %e, provider = primary.id(), "L2 主 provider 摘要失败，尝试降级");
        }
    }

    // 2. 备用 provider（如有）
    if let Some(secondary) = secondary {
        match call_llm_summary(secondary, &input, config).await {
            Ok(summary) => {
                tracing::warn!(
                    provider = secondary.id(),
                    "L2 备用 provider 摘要成功（降级）"
                );
                return Ok(summary);
            }
            Err(e) => tracing::warn!(
                error = %e,
                provider = secondary.id(),
                "L2 备用 provider 摘要失败，降级到启发式兜底"
            ),
        }
    }

    // 3. 启发式兜底（不调 LLM，必成功）
    let summary = heuristic_summary(messages);
    tracing::warn!(
        msg_count = messages.len(),
        "L2 启发式兜底摘要（不调 LLM）：降级链终端"
    );
    Ok(summary)
}

/// 调 LLM 生成摘要，返回摘要文本。
///
/// 构造摘要专用 `ChatRequest`（system 指示精简摘要、`max_output_tokens` 限制长度），
/// 流式收集文本增量。LLM 调用失败时返回 `RuntimeError`（由降级链处理）。
async fn call_llm_summary(
    provider: &dyn LlmProvider,
    input: &str,
    config: &SummarizeConfig,
) -> Result<String, RuntimeError> {
    let system = "You are a summarization assistant. Summarize the following conversation history concisely, preserving key decisions, file paths, and important context. Output only the summary.";
    let user_msg = Message::user_text(format!("Summarize this conversation:\n\n{input}"));

    let req = ChatRequest {
        system: system.to_string(),
        messages: vec![user_msg],
        tools: Vec::new(),
        params: GenerationParams {
            // model 为空：provider 使用自身配置的默认模型（见 openai.rs build_request_body）
            model: String::new(),
            temperature: Some(0.3),
            top_p: None,
            max_output_tokens: Some(config.max_summary_tokens),
            stop: Vec::new(),
            seed: None,
        },
    };

    let mut stream = provider.chat_stream(req).await?;
    let mut summary = String::new();
    while let Some(delta) = stream.next().await {
        match delta? {
            Delta::Text(t) => summary.push_str(&t),
            Delta::Stop(_) => break,
            // 思考过程不进入压缩摘要（与正文分离）
            Delta::ToolCall(_) | Delta::Usage(_) | Delta::Reasoning(_) => {}
        }
    }
    Ok(summary)
}

/// 启发式兜底摘要：每条消息取前 200 字符，用 `; ` 拼接，标注 `[heuristic fallback]`。
///
/// 不调 LLM，纯本地字符串操作，**必成功**。质量低于 LLM 摘要但保证对话不中断。
#[must_use]
fn heuristic_summary(messages: &[&Message]) -> String {
    let parts: Vec<String> = messages
        .iter()
        .map(|m| {
            let text = m.text();
            let truncated: String = text.chars().take(HEURISTIC_CHARS_PER_MSG).collect();
            format!("[{}] {truncated}", role_label(&m.role))
        })
        .collect();
    format!("[heuristic fallback] {}", parts.join("; "))
}

/// 角色标签（用于摘要输入与启发式输出渲染）。
fn role_label(role: &minicoding_core::model::Role) -> &'static str {
    use minicoding_core::model::Role;
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::{LlmError, Message};
    use minicoding_core::provider::{
        BoxFuture, BoxStream, Capabilities, ChatRequest, LlmProvider, Tokenizer,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 可配置的 mock provider：按预设返回摘要文本或错误。
    struct MockProvider {
        id: &'static str,
        /// Ok：返回此文本；Err：返回此错误。
        behavior: MockBehavior,
        call_count: AtomicUsize,
    }

    enum MockBehavior {
        Ok(&'static str),
        Err,
    }

    impl MockProvider {
        fn ok(id: &'static str, text: &'static str) -> Self {
            Self {
                id,
                behavior: MockBehavior::Ok(text),
                call_count: AtomicUsize::new(0),
            }
        }

        fn failing(id: &'static str) -> Self {
            Self {
                id,
                behavior: MockBehavior::Err,
                call_count: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    impl LlmProvider for MockProvider {
        fn id(&self) -> &str {
            self.id
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
            let behavior = &self.behavior;
            Box::pin(async move {
                match behavior {
                    MockBehavior::Ok(text) => {
                        let stream = mock_text_stream(text);
                        Ok(Box::pin(stream) as BoxStream<'static, _>)
                    }
                    MockBehavior::Err => Err(LlmError::Network("mock failure".into())),
                }
            })
        }
        fn count_tokens(&self, messages: &[Message]) -> BoxFuture<'_, usize> {
            let n = messages.len();
            Box::pin(async move { n })
        }
    }

    /// 产生文本增量 + Stop 的 mock stream。
    fn mock_text_stream(
        text: &'static str,
    ) -> impl futures::Stream<Item = Result<Delta, LlmError>> {
        use futures::stream;
        stream::iter(vec![
            Ok(Delta::Text(text.to_string())),
            Ok(Delta::Stop(minicoding_core::model::StopReason::EndTurn)),
        ])
    }

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

    fn make_messages() -> Vec<Message> {
        vec![
            Message::user_text("hello world this is a long user message"),
            Message::assistant_text("hi there, I can help with that"),
        ]
    }

    fn refs(msgs: &[Message]) -> Vec<&Message> {
        msgs.iter().collect()
    }

    #[tokio::test]
    async fn primary_success_skips_fallback() {
        let primary = MockProvider::ok("primary", "LLM summary");
        let config = SummarizeConfig::default();
        let msgs = make_messages();

        let result = summarize_with_fallback(&refs(&msgs), &primary, None, &config)
            .await
            .unwrap();

        assert_eq!(result, "LLM summary");
        assert_eq!(primary.call_count(), 1);
    }

    #[tokio::test]
    async fn primary_fail_secondary_success() {
        let primary = MockProvider::failing("primary");
        let secondary = MockProvider::ok("secondary", "backup summary");
        let config = SummarizeConfig::default();
        let msgs = make_messages();

        let result = summarize_with_fallback(&refs(&msgs), &primary, Some(&secondary), &config)
            .await
            .unwrap();

        assert_eq!(result, "backup summary");
        assert_eq!(primary.call_count(), 1);
        assert_eq!(secondary.call_count(), 1);
    }

    #[tokio::test]
    async fn primary_fail_no_secondary_uses_heuristic() {
        let primary = MockProvider::failing("primary");
        let config = SummarizeConfig::default();
        let msgs = make_messages();

        let result = summarize_with_fallback(&refs(&msgs), &primary, None, &config)
            .await
            .unwrap();

        assert!(result.starts_with("[heuristic fallback]"));
        assert!(result.contains("[user] hello world this is a long user message"));
        assert!(result.contains("[assistant]"));
        assert_eq!(primary.call_count(), 1);
    }

    #[tokio::test]
    async fn all_fail_uses_heuristic() {
        let primary = MockProvider::failing("primary");
        let secondary = MockProvider::failing("secondary");
        let config = SummarizeConfig::default();
        let msgs = make_messages();

        let result = summarize_with_fallback(&refs(&msgs), &primary, Some(&secondary), &config)
            .await
            .unwrap();

        assert!(result.starts_with("[heuristic fallback]"));
        assert_eq!(primary.call_count(), 1);
        assert_eq!(secondary.call_count(), 1);
    }

    #[test]
    fn heuristic_truncates_to_200_chars() {
        let long_text = "x".repeat(500);
        let msg = Message::user_text(long_text);
        let summary = heuristic_summary(&[&msg]);

        assert!(summary.starts_with("[heuristic fallback]"));
        // 截取后应远短于原文
        let user_part = summary
            .strip_prefix("[heuristic fallback] [user] ")
            .unwrap();
        assert_eq!(user_part.chars().count(), HEURISTIC_CHARS_PER_MSG);
    }

    #[test]
    fn heuristic_joins_with_semicolon() {
        let m1 = Message::user_text("first");
        let m2 = Message::assistant_text("second");
        let refs: Vec<&Message> = vec![&m1, &m2];
        let summary = heuristic_summary(&refs);

        assert!(summary.contains("[user] first"));
        assert!(summary.contains("; [assistant] second"));
    }

    #[test]
    fn heuristic_empty_messages() {
        let empty: Vec<&Message> = Vec::new();
        let summary = heuristic_summary(&empty);
        assert_eq!(summary, "[heuristic fallback] ");
    }
}
