//! 重试/限流/超时装饰器（T-M6-3，`features.md` L-05）。
//!
//! [`RetryProvider`] 装饰任意 [`LlmProvider`]，仅对**请求建立阶段**的可重试错误重试
//! （`chat_stream` future resolve 为 `Err`）。流建立后（`Ok(stream)`）的中途错误不重试
//! ——重试会重复已产出内容，违反"不丢已生成内容"语义（C-13）；中途超时由消费者保留
//! 已收到的 delta 后终止。
//!
//! ## 退避策略
//!
//! - 优先用服务端 `Retry-After`（429 携带，`LlmError::retry_after_ms`）；
//! - 缺省时指数退避：`initial_backoff_ms * 2^attempt`，上限 `max_backoff_ms`；
//! - 上限 `max_retries` 次后返回最后一次错误（C-07 资源不可耗尽 / C-13 防死循环）。
//!
//! ## 超时
//!
//! `request_timeout` 限定**单次** `chat_stream` 建立的墙钟时长；超时转为
//! [`LlmError::Timeout`]（可重试）。不限定流式产出总时长（流可长时间产出 token）。

use std::sync::Arc;
use std::time::Duration;

use minicoding_core::model::LlmError;
use minicoding_core::provider::{
    BoxFuture, BoxStream, Capabilities, ChatRequest, Delta, LlmProvider, Tokenizer,
};
use tracing::{debug, warn};

/// 重试配置。
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// 最大重试次数（不含首次尝试，默认 3，C-07）。
    pub max_retries: u32,
    /// 初始退避毫秒（默认 500ms）。
    pub initial_backoff_ms: u64,
    /// 退避上限毫秒（默认 `30_000ms`）。
    pub max_backoff_ms: u64,
    /// 单次请求建立超时（默认 60s；流建立后不受此限制）。
    pub request_timeout: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff_ms: 500,
            max_backoff_ms: 30_000,
            request_timeout: Duration::from_secs(60),
        }
    }
}

/// 重试装饰器：包裹 [`LlmProvider`]，对建立阶段可重试错误做指数退避重试。
///
/// `id`/`capabilities`/`tokenizer`/`count_tokens` 直接委托 inner（装饰器透明）。
/// 仅 `chat_stream` 包裹重试逻辑。
pub struct RetryProvider {
    inner: Arc<dyn LlmProvider>,
    config: RetryConfig,
}

impl std::fmt::Debug for RetryProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryProvider")
            .field("inner_id", &self.inner.id())
            .field("config", &self.config)
            .finish()
    }
}

impl RetryProvider {
    /// 构造装饰器。
    #[must_use]
    pub fn new(inner: Arc<dyn LlmProvider>, config: RetryConfig) -> Self {
        Self { inner, config }
    }

    /// 用默认配置包裹 inner。
    #[must_use]
    pub fn wrap(inner: Arc<dyn LlmProvider>) -> Self {
        Self::new(inner, RetryConfig::default())
    }

    /// 计算第 `attempt` 次重试的退避时长（0-based attempt）。
    ///
    /// 指数退避：`initial * 2^attempt`，上限 `max_backoff_ms`。
    fn backoff(&self, attempt: u32) -> Duration {
        let raw = self
            .config
            .initial_backoff_ms
            .saturating_mul(2u64.saturating_pow(attempt));
        let capped = raw.min(self.config.max_backoff_ms);
        Duration::from_millis(capped)
    }

    /// 单次尝试 + 超时包裹。返回 `Ok(stream)` / `Err(llm_error)`。
    async fn attempt(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError> {
        match tokio::time::timeout(self.config.request_timeout, self.inner.chat_stream(req)).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(LlmError::Timeout(self.config.request_timeout)),
        }
    }
}

impl LlmProvider for RetryProvider {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn tokenizer(&self) -> Arc<dyn Tokenizer> {
        self.inner.tokenizer()
    }

    fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>> {
        Box::pin(async move {
            let mut last_err: Option<LlmError> = None;
            for attempt in 0..=self.config.max_retries {
                let attempt_req = req.clone();
                match self.attempt(attempt_req).await {
                    Ok(stream) => {
                        if attempt > 0 {
                            debug!(
                                target: "minicoding::provider::retry",
                                attempt, "chat_stream 在重试后建立成功"
                            );
                        }
                        return Ok(stream);
                    }
                    Err(e) => {
                        let retryable = e.is_retryable();
                        warn!(
                            target: "minicoding::provider::retry",
                            attempt, error = %e, retryable, "chat_stream 建立失败"
                        );
                        if !retryable || attempt == self.config.max_retries {
                            return Err(e);
                        }
                        // 计算退避：优先 Retry-After，缺省指数退避
                        let delay = e
                            .retry_after_ms()
                            .map_or_else(|| self.backoff(attempt), Duration::from_millis);
                        debug!(
                            target: "minicoding::provider::retry",
                            attempt, ?delay, "退避后重试"
                        );
                        tokio::time::sleep(delay).await;
                        last_err = Some(e);
                    }
                }
            }
            // 理论不可达：循环要么 return Ok，要么 return Err（attempt == max 时）。
            // 此处仅满足类型系统。
            Err(last_err.unwrap_or(LlmError::Network("retry loop exhausted".to_string())))
        })
    }

    fn count_tokens(&self, messages: &[minicoding_core::model::Message]) -> BoxFuture<'_, usize> {
        self.inner.count_tokens(messages)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::needless_pass_by_value)]
    #![allow(clippy::unnecessary_literal_bound)] // trait impl 签名须匹配 trait，不可改 &'static str

    use super::*;
    use futures::stream;
    use minicoding_core::model::Message;
    use minicoding_core::model::ToolSchema;
    use minicoding_core::provider::GenerationParams;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 可编程 mock provider：按调用次数返回预设错误/成功。
    struct MockProvider {
        /// 第 n 次调用返回 `script[n]`。`Err` = 失败，`Ok(())` = 成功（返回空流）。
        script: Mutex<Vec<Result<(), LlmError>>>,
        calls: AtomicU32,
        id: &'static str,
    }

    impl MockProvider {
        fn new(script: Vec<Result<(), LlmError>>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: AtomicU32::new(0),
                id: "mock",
            }
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
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self
                .script
                .lock()
                .expect("script lock")
                .get(n as usize)
                .cloned()
                .unwrap_or(Ok(()));
            Box::pin(async move {
                match outcome {
                    Ok(()) => Ok(Box::pin(stream::empty()) as BoxStream<'static, _>),
                    Err(e) => Err(e),
                }
            })
        }
        fn count_tokens(&self, _messages: &[Message]) -> BoxFuture<'_, usize> {
            Box::pin(async { 0 })
        }
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

    fn req() -> ChatRequest {
        ChatRequest {
            system: String::new(),
            messages: vec![Message::user_text("hi")],
            tools: Vec::<ToolSchema>::new(),
            params: GenerationParams {
                model: "mock".to_string(),
                temperature: None,
                top_p: None,
                max_output_tokens: None,
                stop: vec![],
                seed: None,
            },
        }
    }

    fn fast_config() -> RetryConfig {
        RetryConfig {
            max_retries: 3,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
            request_timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn success_first_try() {
        let mock = Arc::new(MockProvider::new(vec![Ok(())]));
        let calls = Arc::clone(&mock);
        let provider = RetryProvider::wrap(mock as Arc<dyn LlmProvider>);
        let result = provider.chat_stream(req()).await;
        assert!(result.is_ok());
        assert_eq!(calls.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_on_rate_limit_then_succeeds() {
        let mock = Arc::new(MockProvider::new(vec![
            Err(LlmError::RateLimited {
                retry_after_ms: Some(1),
            }),
            Err(LlmError::Server {
                status: 503,
                body: String::new(),
            }),
            Ok(()),
        ]));
        let calls = Arc::clone(&mock);
        let provider = RetryProvider::new(mock as Arc<dyn LlmProvider>, fast_config());
        let result = provider.chat_stream(req()).await;
        assert!(result.is_ok(), "应在重试后成功");
        assert_eq!(calls.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn no_retry_on_client_error() {
        let mock = Arc::new(MockProvider::new(vec![
            Err(LlmError::Client {
                status: 400,
                body: "bad request".to_string(),
            }),
            Ok(()), // 不应被调用
        ]));
        let calls = Arc::clone(&mock);
        let provider = RetryProvider::new(mock as Arc<dyn LlmProvider>, fast_config());
        let result = provider.chat_stream(req()).await;
        assert!(result.is_err());
        assert_eq!(calls.calls.load(Ordering::SeqCst), 1, "4xx 不应重试");
    }

    #[tokio::test]
    async fn exhausts_retries_on_persistent_server_error() {
        let script = vec![
            Err(LlmError::Server {
                status: 500,
                body: String::new()
            });
            10
        ];
        let mock = Arc::new(MockProvider::new(script));
        let calls = Arc::clone(&mock);
        let provider = RetryProvider::new(mock as Arc<dyn LlmProvider>, fast_config());
        let result = provider.chat_stream(req()).await;
        assert!(result.is_err(), "持续 5xx 应在重试上限后失败");
        // 首次 + 3 次重试 = 4 次
        assert_eq!(calls.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn timeout_is_retryable() {
        let mock = Arc::new(MockProvider::new(vec![
            Err(LlmError::Timeout(Duration::from_millis(1))),
            Ok(()),
        ]));
        let calls = Arc::clone(&mock);
        let provider = RetryProvider::new(mock as Arc<dyn LlmProvider>, fast_config());
        let result = provider.chat_stream(req()).await;
        assert!(result.is_ok(), "Timeout 应可重试");
        assert_eq!(calls.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn backoff_uses_retry_after() {
        // Retry-After=2ms，验证优先用服务端建议值（不阻塞测试）。
        let mock = Arc::new(MockProvider::new(vec![
            Err(LlmError::RateLimited {
                retry_after_ms: Some(2),
            }),
            Ok(()),
        ]));
        let provider = RetryProvider::new(mock as Arc<dyn LlmProvider>, fast_config());
        let result = provider.chat_stream(req()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn delegates_non_chat_methods() {
        let mock = Arc::new(MockProvider::new(vec![Ok(())]));
        let provider = RetryProvider::wrap(Arc::clone(&mock) as Arc<dyn LlmProvider>);
        assert_eq!(provider.id(), "mock");
        assert_eq!(provider.capabilities().context_window, 4096);
        let n = provider.count_tokens(&[Message::user_text("hello")]).await;
        assert_eq!(n, 0);
    }
}
