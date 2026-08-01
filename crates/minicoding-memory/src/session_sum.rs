//! 会话摘要：会话结束时生成摘要，供跨会话恢复（注入新会话的 system 段）。
//!
//! 设计要点（见 `docs/design.md` §8、`docs/rules.md` C-29）：
//! - **降级链**：主 provider → 备用 provider（如有）→ 启发式兜底。与 context
//!   compress 的 `summarize_with_fallback` 同构，启发式兜底必成功，**永不向上
//!   抛错**中断会话恢复（C-29：降级链不可跳过）。
//! - **摘要是数据非指令**（C-05）：摘要文本供注入 system 段，由调用方包裹
//!   `<session_summary>` 边界；摘要内容不作为指令执行。
//! - **超时保护**：LLM 调用 30s 超时，超时按失败处理进入降级链。
//! - **启发式兜底**：取每条 user/assistant 消息首 100 字符拼接，标注
//!   `[heuristic fallback]`，纯本地字符串操作。

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use minicoding_core::memory::SessionSummarizer as SessionSummarizerTrait;
use minicoding_core::model::{LlmError, MemoryError, Message, Role};
use minicoding_core::provider::{BoxFuture, ChatRequest, Delta, GenerationParams, LlmProvider};
use minicoding_storage::SessionIndex;

/// 启发式兜底取每条消息首 N 字符（会话摘要规格：100，区别于 context compress 的 200）。
const HEURISTIC_CHARS_PER_MSG: usize = 100;

/// LLM 摘要调用超时（秒）。
const SUMMARY_TIMEOUT_SECS: u64 = 30;

/// 摘要最大字符数（prompt 指示 LLM 生成不超过此长度）。
const MAX_SUMMARY_CHARS: usize = 500;

/// 摘要 `max_output_tokens` 上限（512 tokens 足以覆盖 500 字符摘要）。
const MAX_SUMMARY_TOKENS: usize = 512;

/// 会话摘要生成器实现：持有主/备 LLM provider，按降级链生成摘要。
///
/// 实现 `core::memory::SessionSummarizer` trait。命名遵循 `ProjectDocLoaderImpl`
/// 约定（trait 在 core，struct 在 memory 加 `Impl` 后缀，见 AGENTS.md §3.3）。
///
/// 降级链与 context compress 的 `summarize_with_fallback` 同构（C-29）：
/// 主 provider → 备用 provider（如有）→ 启发式兜底。启发式兜底必成功，故
/// `summarize` 实际不返回 `Err`；保留 `Result` 类型以与管道兼容。
///
/// 摘要内容是数据非指令（C-05）：调用方注入 system 段时应包裹
/// `<session_summary>` 边界。
pub struct SessionSummarizerImpl {
    primary: Arc<dyn LlmProvider>,
    secondary: Option<Arc<dyn LlmProvider>>,
}

impl SessionSummarizerImpl {
    /// 构造摘要生成器。
    ///
    /// `primary` 为主 provider，`secondary` 为备用（降级时使用，`None` 表示无备用）。
    #[must_use]
    pub fn new(primary: Arc<dyn LlmProvider>, secondary: Option<Arc<dyn LlmProvider>>) -> Self {
        Self { primary, secondary }
    }

    /// 生成会话摘要。
    ///
    /// 按降级链尝试：主 provider（30s 超时）→ 备用 provider（如有，30s 超时）
    /// → 启发式兜底。降级时记 `tracing::warn!` 日志。
    ///
    /// # Errors
    /// 启发式兜底恒成功，故实际不返回 `Err`。保留 `Result` 类型以与管道 `?` 兼容，
    /// 并为未来扩展预留。
    pub async fn summarize(&self, messages: &[Message]) -> Result<String, MemoryError> {
        // 渲染待摘要消息为 LLM 输入文本
        let input = messages
            .iter()
            .map(|m| format!("[{}] {}", role_label(&m.role), m.text()))
            .collect::<Vec<_>>()
            .join("\n\n");

        // 1. 主 provider
        match tokio::time::timeout(
            Duration::from_secs(SUMMARY_TIMEOUT_SECS),
            call_llm_summary(self.primary.as_ref(), &input),
        )
        .await
        {
            Ok(Ok(summary)) => return Ok(summary),
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    provider = self.primary.id(),
                    "会话摘要主 provider 失败，尝试降级"
                );
            }
            Err(_) => {
                tracing::warn!(
                    provider = self.primary.id(),
                    timeout_secs = SUMMARY_TIMEOUT_SECS,
                    "会话摘要主 provider 超时，尝试降级"
                );
            }
        }

        // 2. 备用 provider（如有）
        if let Some(secondary) = &self.secondary {
            match tokio::time::timeout(
                Duration::from_secs(SUMMARY_TIMEOUT_SECS),
                call_llm_summary(secondary.as_ref(), &input),
            )
            .await
            {
                Ok(Ok(summary)) => {
                    tracing::warn!(
                        provider = secondary.id(),
                        "会话摘要备用 provider 成功（降级）"
                    );
                    return Ok(summary);
                }
                Ok(Err(e)) => tracing::warn!(
                    error = %e,
                    provider = secondary.id(),
                    "会话摘要备用 provider 失败，降级到启发式兜底"
                ),
                Err(_) => tracing::warn!(
                    provider = secondary.id(),
                    timeout_secs = SUMMARY_TIMEOUT_SECS,
                    "会话摘要备用 provider 超时，降级到启发式兜底"
                ),
            }
        }

        // 3. 启发式兜底（不调 LLM，必成功）
        let summary = heuristic_summary(messages);
        tracing::warn!(
            msg_count = messages.len(),
            "会话摘要启发式兜底（不调 LLM）：降级链终端"
        );
        Ok(summary)
    }
}

impl SessionSummarizerTrait for SessionSummarizerImpl {
    fn summarize<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<String, MemoryError>> {
        Box::pin(async move {
            // 复用 inherent method（同逻辑，trait impl 仅做分发）
            SessionSummarizerImpl::summarize(self, messages).await
        })
    }
}

/// 调 LLM 生成摘要，返回摘要文本。
///
/// 构造摘要专用 `ChatRequest`（system 指示精简摘要、`max_output_tokens` 限制长度），
/// 流式收集文本增量。LLM 调用失败时返回 `LlmError`（由降级链处理）。
async fn call_llm_summary(provider: &dyn LlmProvider, input: &str) -> Result<String, LlmError> {
    let system = format!(
        "You are a summarization assistant. Summarize the following conversation concisely (max {MAX_SUMMARY_CHARS} characters), preserving key decisions, file paths, and important context. Output only the summary."
    );
    let user_msg = Message::user_text(format!("Summarize this conversation:\n\n{input}"));

    let req = ChatRequest {
        system,
        messages: vec![user_msg],
        tools: Vec::new(),
        params: GenerationParams {
            // model 为空：provider 使用自身配置的默认模型
            model: String::new(),
            temperature: Some(0.3),
            top_p: None,
            max_output_tokens: Some(MAX_SUMMARY_TOKENS),
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
            Delta::ToolCall(_) | Delta::Usage(_) => {}
        }
    }
    Ok(summary)
}

/// 启发式兜底摘要：每条 user/assistant 消息取前 100 字符，用 `; ` 拼接，
/// 标注 `[heuristic fallback]`。
///
/// 不调 LLM，纯本地字符串操作，**必成功**。仅纳入 user/assistant 消息
/// （system/tool 消息对跨会话恢复无意义）。
#[must_use]
fn heuristic_summary(messages: &[Message]) -> String {
    let parts: Vec<String> = messages
        .iter()
        .filter(|m| matches!(m.role, Role::User | Role::Assistant))
        .map(|m| {
            let text = m.text();
            let truncated: String = text.chars().take(HEURISTIC_CHARS_PER_MSG).collect();
            format!("[{}] {truncated}", role_label(&m.role))
        })
        .collect();
    format!("[heuristic fallback] {}", parts.join("; "))
}

/// 角色标签（用于摘要输入与启发式输出渲染）。
fn role_label(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// 将摘要写入会话索引：更新指定 `session_id` 的 `summary` 字段。
///
/// 若 `session_id` 不在索引中则无操作（会话尚未建立索引项时不应调用本函数）。
/// 已存在则覆盖原 `summary`（无论原值是否为 `None`），保留原 `created_at`。
pub fn save_summary(index: &mut SessionIndex, session_id: &str, summary: String) {
    let updated = {
        let Some(entry) = index.get(session_id) else {
            return;
        };
        let mut u = entry.clone();
        u.summary = Some(summary);
        u
    };
    index.add(updated);
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::{LlmError, Message, StopReason};
    use minicoding_core::provider::{
        BoxFuture, BoxStream, Capabilities, ChatRequest, LlmProvider, Tokenizer,
    };
    use minicoding_storage::SessionIndexEntry;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use time::OffsetDateTime;

    /// 可配置的 mock provider：按预设返回摘要文本或错误。
    /// 调用计数通过共享 `Arc<AtomicUsize>` 暴露给测试，便于在 provider 被转为
    /// `Arc<dyn LlmProvider>` 后仍可校验降级链触达情况。
    struct MockProvider {
        id: &'static str,
        behavior: MockBehavior,
        call_count: Arc<AtomicUsize>,
    }

    enum MockBehavior {
        Ok(&'static str),
        Err,
    }

    impl MockProvider {
        /// 构造成功返回固定文本的 mock，返回 `(provider, 调用计数共享句柄)`。
        fn ok(id: &'static str, text: &'static str) -> (Arc<dyn LlmProvider>, Arc<AtomicUsize>) {
            let counter = Arc::new(AtomicUsize::new(0));
            let provider: Arc<dyn LlmProvider> = Arc::new(Self {
                id,
                behavior: MockBehavior::Ok(text),
                call_count: Arc::clone(&counter),
            });
            (provider, counter)
        }

        /// 构造恒失败的 mock，返回 `(provider, 调用计数共享句柄)`。
        fn failing(id: &'static str) -> (Arc<dyn LlmProvider>, Arc<AtomicUsize>) {
            let counter = Arc::new(AtomicUsize::new(0));
            let provider: Arc<dyn LlmProvider> = Arc::new(Self {
                id,
                behavior: MockBehavior::Err,
                call_count: Arc::clone(&counter),
            });
            (provider, counter)
        }
    }

    impl LlmProvider for MockProvider {
        fn id(&self) -> &'static str {
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
            Ok(Delta::Stop(StopReason::EndTurn)),
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

    #[tokio::test]
    async fn primary_success_skips_fallback() {
        let (primary, primary_calls) = MockProvider::ok("primary", "LLM summary");
        let summarizer = SessionSummarizerImpl::new(primary, None);
        let msgs = make_messages();

        let result = summarizer.summarize(&msgs).await.unwrap();

        assert_eq!(result, "LLM summary");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn primary_fail_secondary_success() {
        let (primary, primary_calls) = MockProvider::failing("primary");
        let (secondary, secondary_calls) = MockProvider::ok("secondary", "backup summary");
        let summarizer = SessionSummarizerImpl::new(primary, Some(secondary));
        let msgs = make_messages();

        let result = summarizer.summarize(&msgs).await.unwrap();

        assert_eq!(result, "backup summary");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(secondary_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn primary_fail_no_secondary_uses_heuristic() {
        let (primary, primary_calls) = MockProvider::failing("primary");
        let summarizer = SessionSummarizerImpl::new(primary, None);
        let msgs = make_messages();

        let result = summarizer.summarize(&msgs).await.unwrap();

        assert!(result.starts_with("[heuristic fallback]"));
        assert!(result.contains("[user] hello world this is a long user message"));
        assert!(result.contains("[assistant]"));
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn all_fail_uses_heuristic() {
        let (primary, primary_calls) = MockProvider::failing("primary");
        let (secondary, secondary_calls) = MockProvider::failing("secondary");
        let summarizer = SessionSummarizerImpl::new(primary, Some(secondary));
        let msgs = make_messages();

        let result = summarizer.summarize(&msgs).await.unwrap();

        assert!(result.starts_with("[heuristic fallback]"));
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(secondary_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn heuristic_filters_user_assistant_only() {
        let msgs = vec![
            Message::system_text("system instruction that should be excluded"),
            Message::user_text("user question"),
            Message::assistant_text("assistant answer"),
        ];
        let summary = heuristic_summary(&msgs);

        assert!(summary.starts_with("[heuristic fallback]"));
        assert!(
            !summary.contains("[system]"),
            "system messages must be excluded from heuristic summary"
        );
        assert!(summary.contains("[user] user question"));
        assert!(summary.contains("[assistant] assistant answer"));
    }

    #[test]
    fn heuristic_truncates_to_100_chars() {
        let long_text = "x".repeat(500);
        let msg = Message::user_text(long_text);
        let summary = heuristic_summary(std::slice::from_ref(&msg));

        assert!(summary.starts_with("[heuristic fallback]"));
        let user_part = summary
            .strip_prefix("[heuristic fallback] [user] ")
            .unwrap();
        assert_eq!(user_part.chars().count(), HEURISTIC_CHARS_PER_MSG);
    }

    #[test]
    fn heuristic_empty_messages() {
        let empty: Vec<Message> = Vec::new();
        let summary = heuristic_summary(&empty);
        assert_eq!(summary, "[heuristic fallback] ");
    }

    #[test]
    fn save_summary_updates_existing_entry() {
        let mut index = SessionIndex::new();
        let entry = SessionIndexEntry::new("sess-1", None, OffsetDateTime::now_utc());
        index.add(entry);

        save_summary(&mut index, "sess-1", "new summary".to_string());

        let updated = index.get("sess-1").unwrap();
        assert_eq!(updated.summary.as_deref(), Some("new summary"));
    }

    #[test]
    fn save_summary_overwrites_existing_summary() {
        let mut index = SessionIndex::new();
        let entry =
            SessionIndexEntry::new("sess-1", Some("old".to_string()), OffsetDateTime::now_utc());
        index.add(entry);

        save_summary(&mut index, "sess-1", "fresh".to_string());

        let updated = index.get("sess-1").unwrap();
        assert_eq!(updated.summary.as_deref(), Some("fresh"));
    }

    #[test]
    fn save_summary_missing_session_noop() {
        let mut index = SessionIndex::new();
        save_summary(&mut index, "nonexistent", "summary".to_string());
        assert!(index.is_empty());
    }
}
