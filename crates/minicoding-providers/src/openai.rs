//! `OpenAI` 兼容 provider（支持 `OpenAI` / Azure `OpenAI` / Ollama 等 `OpenAI` 风格 API）。
//!
//! 通过 `reqwest` 发起 POST `{api_base}/chat/completions`，`stream: true`，按 SSE
//! 协议解析响应，转换为 [`Delta`]。HTTP 状态码映射到 [`LlmError`]：
//! 429 → `RateLimited`（携带 `Retry-After`），5xx → `Server`，其它 4xx → `Client`。

use futures::StreamExt;
use minicoding_core::model::{ContentBlock, LlmError, Message, Role, StopReason, ToolContent};
use minicoding_core::provider::{
    BoxFuture, BoxStream, Capabilities, ChatRequest, Delta, LlmProvider, Tokenizer, ToolCallDelta,
    Usage,
};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::debug;

use crate::tokenizer::TiktokenTokenizer;

/// Provider 标识。
pub const PROVIDER_ID: &str = "openai";

/// `OpenAI` 兼容 LLM provider。
///
/// 构造后通过 `Arc<dyn LlmProvider>` 注入 Runtime。所有方法返回 `BoxFuture` /
/// `BoxStream`，保证 `dyn` 兼容（见 `core::provider::trait`）。
pub struct OpenAiProvider {
    /// 自定义显示名（`None` 时回退到 `PROVIDER_ID`）。
    display_name: Option<String>,
    api_base: String,
    /// M-10：凭证重解析器（每次请求 resolve，缓存 ≤TTL；不再持有构造期一次性快照）。
    resolver: Arc<crate::common::CredentialResolver>,
    model: String,
    client: reqwest::Client,
    tokenizer: Arc<TiktokenTokenizer>,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("display_name", &self.display_name)
            .field("api_base", &self.api_base)
            .field("model", &self.model)
            .field("tokenizer", &self.tokenizer.kind())
            // 不输出凭证内容（C-04：日志脱敏）
            .field("api_key", &"<resolver>")
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// 构造 provider。
    ///
    /// `api_base` 形如 `https://api.openai.com/v1`，无需尾部 `/`；`model` 决定分词器与
    /// 请求中的 `model` 字段。`display_name` 为自定义显示名（`None` 时回退到 `"openai"`）。
    ///
    /// # Errors
    /// - `reqwest::Client` 初始化失败 → [`LlmError::Network`]
    /// - 分词器加载失败 → [`LlmError::Parse`]
    pub fn new(
        api_base: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        Self::with_name(None, api_base, api_key, model)
    }

    /// 构造 provider 并指定自定义显示名。
    ///
    /// `display_name` 为 `None` 时 `id()` 回退到 `PROVIDER_ID`（`"openai"`）；
    /// 为 `Some("deepseek")` 时 `id()` 返回 `"deepseek"`，用于日志/metrics 维度区分。
    ///
    /// # Errors
    /// - `reqwest::Client` 初始化失败 → [`LlmError::Network`]
    /// - 分词器加载失败 → [`LlmError::Parse`]
    pub fn with_name(
        display_name: Option<String>,
        api_base: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        let model_str = model.into();
        let tokenizer = TiktokenTokenizer::new_for_model(&model_str).map_err(LlmError::Parse)?;
        // 读超时（2026-08-23 审查 §5-P2）：此前未设任何超时——服务端建立连接
        // 后停止发送数据会导致消费端永久挂起（RetryProvider 的超时仅覆盖建立
        // 阶段）。取宽裕值 300s：容忍推理模型静默思考期；空闲超过即判死。
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .read_timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;
        let resolver = crate::common::CredentialResolver::from_env();
        let key = api_key.into();
        if !key.is_empty() {
            resolver.seed(PROVIDER_ID, key);
        }
        Ok(Self {
            display_name,
            api_base: api_base.into(),
            resolver: Arc::new(resolver),
            model: model_str,
            client,
            tokenizer: Arc::new(tokenizer),
        })
    }

    /// 构造 POST 请求体（OpenAI chat completions 格式，`stream: true`）。
    // 保留 `&self` 接收者（`unused_self`）：M-12 起 model 取自 `req.params.model`
    // （turn 边界热换 model），改为关联函数会波及多处测试调用点；`&self` 风格与
    // `message_to_openai` 等辅助保持一致。
    #[allow(clippy::unused_self)]
    fn build_request_body(&self, req: &ChatRequest) -> Value {
        let mut messages = Vec::with_capacity(req.messages.len() + 1);
        if !req.system.is_empty() {
            messages.push(json!({"role": "system", "content": req.system}));
        }
        for m in &req.messages {
            messages.push(message_to_openai(m));
        }

        let mut body = json!({
            "model": req.params.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });

        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = Value::Array(tools);
        }

        if let Some(t) = req.params.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(t) = req.params.top_p {
            body["top_p"] = json!(t);
        }
        if let Some(m) = req.params.max_output_tokens {
            body["max_tokens"] = json!(m);
        }
        if !req.params.stop.is_empty() {
            body["stop"] = json!(req.params.stop);
        }
        if let Some(seed) = req.params.seed {
            body["seed"] = json!(seed);
        }
        body
    }

    /// 构造鉴权 headers（M-10：每次请求经 resolver 重解析凭证，换 key 零重启）。
    fn auth_headers(&self) -> Result<HeaderMap, LlmError> {
        let key = self
            .resolver
            .resolve(PROVIDER_ID)?
            .ok_or(LlmError::NotConfigured)?;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let bearer = format!("Bearer {key}");
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&bearer)
                .map_err(|e| LlmError::Network(format!("invalid api key: {e}")))?,
        );
        Ok(headers)
    }
}

impl LlmProvider for OpenAiProvider {
    fn id(&self) -> &str {
        self.display_name.as_deref().unwrap_or(PROVIDER_ID)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tool_call: true,
            supports_vision: false,
            supports_streaming: true,
            supports_json_mode: true,
            context_window: 128_000,
            max_output: 4_096,
        }
    }

    fn tokenizer(&self) -> Arc<dyn Tokenizer> {
        self.tokenizer.clone()
    }

    fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>> {
        Box::pin(async move {
            let body = self.build_request_body(&req);
            let url = format!("{}/chat/completions", self.api_base.trim_end_matches('/'));

            debug!(target: "minicoding::provider::openai", model = %self.model, url = %url, "POST chat/completions stream");

            // Q4：发送/状态检查/行解码统一走 common::stream_runner；
            // `[DONE]` 哨兵是 OpenAI 特有语义，保留在 provider 侧过滤。
            let request = self
                .client
                .post(&url)
                .headers(self.auth_headers()?)
                .json(&body);
            let resp =
                crate::common::stream_runner::send_and_check(request, |status, body, headers| {
                    map_status_error(status, body, retry_after_ms(headers))
                })
                .await?;

            // SSE data payload：先滤 `[DONE]` 哨兵（OpenAI 特有），其余交共享管道解析
            let sse = crate::common::sse::from_response(resp);
            let filtered = sse.filter(|ev| {
                let keep = !matches!(ev, Ok(data) if data == "[DONE]");
                std::future::ready(keep)
            });
            let delta_stream =
                crate::common::stream_runner::lines_to_deltas(Box::pin(filtered), parse_chunk);

            Ok(Box::pin(delta_stream) as BoxStream<'static, _>)
        })
    }

    fn count_tokens(&self, messages: &[Message]) -> BoxFuture<'_, usize> {
        let n = self.tokenizer.count_messages(messages);
        Box::pin(async move { n })
    }
}

/// 将 [`Message`] 映射到 `OpenAI` chat completions wire format。
fn message_to_openai(m: &Message) -> Value {
    let role = role_str(&m.role);
    let text = extract_text(&m.content);

    // tool 响应消息：role=tool + tool_call_id + content
    // C-05：工具结果回灌 LLM 时包裹 `<tool_output>` 边界，防 LLM 把输出当指令执行。
    if m.role == Role::Tool {
        let mut obj = serde_json::Map::new();
        obj.insert("role".to_string(), Value::String(role.to_string()));
        // tool_call_id 优先取消息字段；运行时构造的 tool 消息只填在
        // `ContentBlock::ToolResult.call_id`（见 rt.rs tool_result_message），缺失时回退。
        let call_id = crate::common::tool_call_id_of(m);
        if let Some(call_id) = call_id {
            obj.insert("tool_call_id".to_string(), Value::String(call_id));
        }
        obj.insert(
            "content".to_string(),
            Value::String(crate::common::wrap_tool_output(&text)),
        );
        return Value::Object(obj);
    }

    // assistant + tool_calls
    if m.role == Role::Assistant && !m.tool_calls.is_empty() {
        let tool_calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|tc| {
                json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.input.to_string(),
                    }
                })
            })
            .collect();
        let mut obj = serde_json::Map::new();
        obj.insert("role".to_string(), Value::String(role.to_string()));
        obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
        if text.is_empty() {
            obj.insert("content".to_string(), Value::Null);
        } else {
            obj.insert("content".to_string(), Value::String(text));
        }
        return Value::Object(obj);
    }

    // 默认 system / user / assistant 纯文本
    json!({"role": role, "content": text})
}

/// 不支持视觉的 provider 收到图片块时的占位文本（C-05：显式告知而非静默丢弃）。
const IMAGE_OMITTED_PLACEHOLDER: &str = "[image omitted: 当前模型通道不支持图片输入]";

/// 从 `ContentBlock` 列表提取文本（含 `ToolResult` 内容；忽略冗余 `ToolUse`）。
///
/// `Image` 块替换为占位文本而非静默丢弃（2026-08-23 审查 §5-P2）：静默丢弃
/// 会让 LLM 看到的对话凭空少一块内容且无任何提示，模型可能对"未提供的图"
/// 产生幻觉应答。
fn extract_text(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: t } => parts.push(t.clone()),
            ContentBlock::ToolResult { content, .. } => {
                parts.push(tool_content_to_string(content));
            }
            ContentBlock::Image { .. } => {
                parts.push(IMAGE_OMITTED_PLACEHOLDER.to_string());
            }
            ContentBlock::ToolUse(_) => {}
        }
    }
    parts.join("\n")
}

/// 将 [`ToolContent`] 序列化为字符串（OpenAI tool 响应只接受 string content）。
fn tool_content_to_string(content: &ToolContent) -> String {
    match content {
        ToolContent::Text(s) => s.clone(),
        ToolContent::Json(v) => v.to_string(),
        ToolContent::Image { .. } => String::new(),
        ToolContent::Mixed(parts) => parts
            .iter()
            .map(tool_content_to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// 返回 role 的小写字符串表示。
fn role_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// HTTP 状态码 → [`LlmError`]。
fn map_status_error(status: u16, body: String, retry_after_ms: Option<u64>) -> LlmError {
    match status {
        429 => LlmError::RateLimited { retry_after_ms },
        s if (500..600).contains(&s) => LlmError::Server { status: s, body },
        s => LlmError::Client { status: s, body },
    }
}

/// 从 `Retry-After` header 解析重试毫秒数（仅支持秒数形式）。
fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000))
}

/// 解析 `OpenAI` chunk JSON，转换为零到多个 [`Delta`]。
///
/// 单个 chunk 可能同时包含 `delta.content` 与 `delta.tool_calls`（多个分片），统一展开为
/// 顺序 `Delta`。`finish_reason` 出现时附 `Delta::Stop`；`usage` 出现时附 `Delta::Usage`。
fn parse_chunk(chunk: &Value) -> Vec<Delta> {
    let mut deltas = Vec::new();

    if let Some(choices) = chunk.get("choices").and_then(Value::as_array)
        && let Some(choice) = choices.first()
    {
        if let Some(delta) = choice.get("delta") {
            // 思考过程：DeepSeek 用 `reasoning_content`，OpenAI o 系列用 `reasoning`
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                deltas.push(Delta::Reasoning(reasoning.to_string()));
            }
            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                deltas.push(Delta::Text(content.to_string()));
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    let index = u32_from_json(tc.get("index"));
                    let id = tc.get("id").and_then(Value::as_str).map(String::from);
                    let function = tc.get("function");
                    let name = function
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    let args_chunk = function
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    deltas.push(Delta::ToolCall(ToolCallDelta {
                        index,
                        id,
                        name,
                        args_chunk,
                    }));
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str)
            && !reason.is_empty()
        {
            deltas.push(Delta::Stop(map_stop_reason(reason)));
        }
    }

    if let Some(usage) = chunk.get("usage") {
        deltas.push(Delta::Usage(parse_usage(usage)));
    }

    deltas
}

/// 解析 `OpenAI` `usage` 对象为 [`Usage`]。
fn parse_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usize_from_json(usage.get("prompt_tokens")),
        output_tokens: usize_from_json(usage.get("completion_tokens")),
        cache_read: usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(usize_from_option),
        cache_write: None,
    }
}

/// 从 JSON number 取 `u32`，超界或缺失时返回 0。
fn u32_from_json(v: Option<&Value>) -> u32 {
    v.and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// 从 JSON number 取 `usize`，超界或缺失时返回 0。
fn usize_from_json(v: Option<&Value>) -> usize {
    v.and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0)
}

/// 从 `&Value` 取 `usize`，超界返回 `None`（用于 `.and_then` 链）。
fn usize_from_option(v: &Value) -> Option<usize> {
    v.as_u64().and_then(|n| usize::try_from(n).ok())
}

/// `OpenAI` `finish_reason` → [`StopReason`]。
fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        _ => StopReason::Stopped,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::needless_pass_by_value)]

    use super::*;
    use futures::stream::StreamExt;
    use minicoding_core::model::{ToolCall as ModelToolCall, ToolSchema};
    use minicoding_core::provider::GenerationParams;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// 构造最小 `ChatRequest`（无 system、无 tools、无生成参数）。
    fn basic_req() -> ChatRequest {
        ChatRequest {
            system: String::new(),
            messages: vec![Message::user_text("hi")],
            tools: Vec::<ToolSchema>::new(),
            params: GenerationParams {
                model: "gpt-4".to_string(),
                temperature: None,
                top_p: None,
                max_output_tokens: None,
                stop: vec![],
                seed: None,
                thinking_budget_tokens: None,
            },
        }
    }

    /// 把单个 JSON 值包装为 SSE `data:` 事件行。
    fn sse_event(json: &Value) -> String {
        format!("data: {json}\n\n")
    }

    /// 收集 `BoxStream` 所有 Ok delta（遇 Err 则 panic，便于定位）。
    async fn collect_deltas(stream: BoxStream<'static, Result<Delta, LlmError>>) -> Vec<Delta> {
        let mut out = Vec::new();
        let mut s = stream;
        while let Some(item) = s.next().await {
            match item {
                Ok(d) => out.push(d),
                Err(e) => panic!("未预期的 delta 错误: {e:?}"),
            }
        }
        out
    }

    // --- parse_chunk ---

    #[test]
    fn parse_chunk_content_delta_emits_text() {
        let chunk = json!({"choices": [{"delta": {"content": "hello"}, "index": 0}]});
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Text(t) if t == "hello"));
    }

    #[test]
    fn parse_chunk_empty_content_skipped() {
        let chunk = json!({"choices": [{"delta": {"content": ""}}]});
        assert!(parse_chunk(&chunk).is_empty());
    }

    #[test]
    fn parse_chunk_tool_call_delta_emits_toolcall() {
        let args = "{\"path\":";
        let chunk = json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_1",
            "function": {"name": "fs.read", "arguments": args}
        }]}}]});
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 0);
                assert_eq!(tc.id.as_deref(), Some("call_1"));
                assert_eq!(tc.name.as_deref(), Some("fs.read"));
                assert_eq!(tc.args_chunk.as_deref(), Some("{\"path\":"));
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_chunk_content_and_tool_call_in_same_delta() {
        // 单个 delta 可同时含 content 与 tool_calls，展开为顺序 Delta
        let chunk = json!({"choices": [{"delta": {
            "content": "thinking",
            "tool_calls": [{"index": 1, "function": {"name": "calc"}}]
        }}]});
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 2);
        assert!(matches!(&deltas[0], Delta::Text(t) if t == "thinking"));
        assert!(matches!(&deltas[1], Delta::ToolCall(tc) if tc.index == 1));
    }

    #[test]
    fn parse_chunk_finish_reason_emits_stop() {
        let chunk = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Stop(StopReason::EndTurn)));
    }

    #[test]
    fn parse_chunk_usage_emits_usage() {
        let chunk = json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 3}
            }
        });
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            Delta::Usage(u) => {
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 5);
                assert_eq!(u.cache_read, Some(3));
                assert!(u.cache_write.is_none(), "OpenAI 不返回 cache_write");
            }
            other => panic!("期望 Usage，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_chunk_no_choices_only_usage() {
        let chunk = json!({"usage": {"prompt_tokens": 1, "completion_tokens": 1}});
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Usage(_)));
    }

    #[test]
    fn parse_chunk_empty_choices_no_usage_yields_nothing() {
        let chunk = json!({"choices": []});
        assert!(parse_chunk(&chunk).is_empty());
    }

    // --- map_stop_reason ---

    #[test]
    fn map_stop_reason_variants() {
        assert_eq!(map_stop_reason("stop"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("length"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("function_call"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("unknown"), StopReason::Stopped);
    }

    // --- map_status_error ---

    #[test]
    fn map_status_error_categories() {
        assert!(matches!(
            map_status_error(401, "unauth".into(), None),
            LlmError::Client { status: 401, .. }
        ));
        assert!(matches!(
            map_status_error(429, String::new(), Some(500)),
            LlmError::RateLimited {
                retry_after_ms: Some(500)
            }
        ));
        assert!(matches!(
            map_status_error(500, "err".into(), None),
            LlmError::Server { status: 500, .. }
        ));
        assert!(matches!(
            map_status_error(404, "nf".into(), None),
            LlmError::Client { status: 404, .. }
        ));
    }

    // --- retry_after_ms ---

    #[test]
    fn retry_after_ms_parses_seconds_to_millis() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(retry_after_ms(&headers), Some(2000));
    }

    #[test]
    fn retry_after_ms_missing_returns_none() {
        assert_eq!(retry_after_ms(&HeaderMap::new()), None);
    }

    #[test]
    fn retry_after_ms_invalid_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("abc"));
        assert_eq!(retry_after_ms(&headers), None);
    }

    // --- parse_usage ---

    #[test]
    fn parse_usage_missing_fields_default_zero() {
        let u = parse_usage(&json!({}));
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
        assert!(u.cache_read.is_none());
        assert!(u.cache_write.is_none());
    }

    // --- u32 / usize helpers ---

    #[test]
    fn u32_from_json_boundaries() {
        assert_eq!(u32_from_json(Some(&json!(0))), 0);
        assert_eq!(u32_from_json(Some(&json!(42))), 42);
        // 超界回 0
        assert_eq!(u32_from_json(Some(&json!(u64::MAX))), 0);
        assert_eq!(u32_from_json(None), 0);
        assert_eq!(u32_from_json(Some(&json!("not a number"))), 0);
    }

    #[test]
    fn usize_from_option_boundaries() {
        assert_eq!(usize_from_option(&json!(5)), Some(5));
        assert_eq!(usize_from_option(&json!(0)), Some(0));
        assert_eq!(usize_from_option(&json!("x")), None);
    }

    // --- message_to_openai ---

    #[test]
    fn message_to_openai_user_text() {
        let v = message_to_openai(&Message::user_text("hi"));
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "hi");
    }

    #[test]
    fn message_to_openai_system_text() {
        let v = message_to_openai(&Message::system_text("rules"));
        assert_eq!(v["role"], "system");
        assert_eq!(v["content"], "rules");
    }

    #[test]
    fn message_to_openai_tool_result() {
        let mut msg = Message::user_text("result");
        msg.role = Role::Tool;
        msg.tool_call_id = Some("call_1".into());
        let v = message_to_openai(&msg);
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_1");
        // C-05：工具结果回灌 LLM 时包裹 `<tool_output>` 边界
        assert_eq!(v["content"], "<tool_output>\nresult\n</tool_output>");
    }

    #[test]
    fn message_to_openai_assistant_with_tool_calls() {
        let mut msg = Message::assistant_text("thinking");
        msg.tool_calls = vec![ModelToolCall {
            id: "call_2".into(),
            name: "fs.read".into(),
            input: json!({"path": "/tmp"}),
        }];
        let v = message_to_openai(&msg);
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"], "thinking");
        assert_eq!(v["tool_calls"][0]["id"], "call_2");
        assert_eq!(v["tool_calls"][0]["type"], "function");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "fs.read");
        // OpenAI 风格：arguments 是 JSON 字符串（非对象）
        assert_eq!(
            v["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"/tmp\"}"
        );
    }

    #[test]
    fn message_to_openai_assistant_no_text_null_content() {
        // assistant 仅有 tool_calls 且无文本时，content 为 null
        let mut msg = Message::assistant_text("");
        msg.tool_calls = vec![ModelToolCall {
            id: "call_3".into(),
            name: "noop".into(),
            input: json!({}),
        }];
        let v = message_to_openai(&msg);
        assert_eq!(v["role"], "assistant");
        assert!(v["content"].is_null());
        assert_eq!(v["tool_calls"][0]["function"]["name"], "noop");
    }

    // --- build_request_body ---

    #[test]
    fn build_request_body_structure() {
        let provider = OpenAiProvider::new("https://api.openai.com/v1", "sk-test", "gpt-4")
            .expect("构造 provider");
        let req = ChatRequest {
            system: "you are helpful".to_string(),
            messages: vec![Message::user_text("hi")],
            tools: vec![ToolSchema {
                name: "fs.read".to_string(),
                description: "read a file".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            params: GenerationParams {
                model: "gpt-4".to_string(),
                temperature: Some(0.5),
                top_p: Some(0.9),
                max_output_tokens: Some(1024),
                stop: vec!["END".to_string()],
                seed: Some(42),
                thinking_budget_tokens: None,
            },
        };
        let body = provider.build_request_body(&req);
        assert_eq!(body["model"], "gpt-4");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        // system 作为第一条 message
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "you are helpful");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hi");
        // tools
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "fs.read");
        assert_eq!(body["tools"][0]["function"]["description"], "read a file");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
        // params（f32 精度：0.9 在 f32 中为 0.8999999761581421，用 f32 字面量比较）
        assert_eq!(body["temperature"], json!(0.5_f32));
        assert_eq!(body["top_p"], json!(0.9_f32));
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["stop"], json!(["END"]));
        assert_eq!(body["seed"], 42);
    }

    #[test]
    fn build_request_body_no_system_no_tools_minimal() {
        let provider = OpenAiProvider::new("https://api.openai.com/v1", "sk-test", "gpt-4")
            .expect("构造 provider");
        let body = provider.build_request_body(&basic_req());
        assert_eq!(body["model"], "gpt-4");
        assert_eq!(body["stream"], true);
        // 无 system 时 messages 仅含用户消息
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
        // 无 tools 时不出现 tools 字段
        assert!(body.get("tools").is_none());
        // 无可选 params 时不出现对应字段
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    // --- auth_headers ---

    #[test]
    fn auth_headers_includes_bearer_and_content_type() {
        let provider = OpenAiProvider::new("https://api.openai.com/v1", "sk-test-key", "gpt-4")
            .expect("构造 provider");
        let headers = provider.auth_headers().expect("构造 headers");
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer sk-test-key");
    }

    // --- chat_stream HTTP mock 测试 ---

    #[tokio::test]
    async fn chat_stream_parses_sse_content_to_text_delta() {
        // 场景：mock 返回 SSE 流含 content delta → 解析为 Delta::Text；[DONE] 终止流
        let server = MockServer::start().await;
        let chunk = json!({"choices": [{"delta": {"content": "hello"}, "index": 0}]});
        let sse_body = format!("{}data: [DONE]\n\n", sse_event(&chunk));
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "sk-test", "gpt-4").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        // [DONE] 不产出 delta；content → 单个 Text
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Text(t) if t == "hello"));
    }

    #[tokio::test]
    async fn chat_stream_parses_tool_call_delta() {
        // 场景：SSE 流含 tool_calls delta → 解析为 Delta::ToolCall
        let server = MockServer::start().await;
        let args = "{\"path\":\"/tmp\"}";
        let chunk = json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_1",
            "function": {"name": "fs.read", "arguments": args}
        }]}}]});
        let sse_body = format!("{}data: [DONE]\n\n", sse_event(&chunk));
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "sk-test", "gpt-4").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 0);
                assert_eq!(tc.id.as_deref(), Some("call_1"));
                assert_eq!(tc.name.as_deref(), Some("fs.read"));
                assert_eq!(tc.args_chunk.as_deref(), Some(args));
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_done_sentinel_terminates_cleanly() {
        // 场景：SSE 流以 [DONE] 结束 → 流正常终止，无错误
        let server = MockServer::start().await;
        let sse_body = "data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "sk-test", "gpt-4").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        // [DONE] 不产出任何 delta
        assert!(deltas.is_empty(), "expected empty: deltas");
    }

    #[tokio::test]
    async fn chat_stream_emits_stop_and_usage() {
        // 场景：SSE 流含 finish_reason 与 usage → Delta::Stop + Delta::Usage
        let server = MockServer::start().await;
        let text_chunk = json!({"choices": [{"delta": {"content": "ok"}, "index": 0}]});
        let stop_chunk = json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 8, "completion_tokens": 2}
        });
        let sse_body = format!(
            "{}{}data: [DONE]\n\n",
            sse_event(&text_chunk),
            sse_event(&stop_chunk)
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "sk-test", "gpt-4").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        assert_eq!(deltas.len(), 3);
        assert!(matches!(&deltas[0], Delta::Text(t) if t == "ok"));
        assert!(matches!(&deltas[1], Delta::Stop(StopReason::ToolUse)));
        match &deltas[2] {
            Delta::Usage(u) => {
                assert_eq!(u.input_tokens, 8);
                assert_eq!(u.output_tokens, 2);
            }
            other => panic!("期望 Usage，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_401_returns_client_error() {
        // 场景：HTTP 401 鉴权失败 → LlmError::Client
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "sk-test", "gpt-4").expect("构造 provider");
        let result = provider.chat_stream(basic_req()).await;
        let Err(err) = result else {
            panic!("401 应返回错误，但 chat_stream 成功");
        };
        match err {
            LlmError::Client { status, body } => {
                assert_eq!(status, 401);
                assert_eq!(body, "unauthorized");
            }
            other => panic!("期望 Client 错误，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_429_returns_rate_limited_with_retry_after() {
        // 场景：HTTP 429 限流 + Retry-After → LlmError::RateLimited（携带毫秒）
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_string("slow down")
                    .insert_header("retry-after", "5"),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "sk-test", "gpt-4").expect("构造 provider");
        let result = provider.chat_stream(basic_req()).await;
        let Err(err) = result else {
            panic!("429 应返回错误，但 chat_stream 成功");
        };
        match err {
            LlmError::RateLimited { retry_after_ms } => {
                assert_eq!(retry_after_ms, Some(5000));
            }
            other => panic!("期望 RateLimited 错误，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_connection_refused_returns_network_error() {
        // 场景：网络错误（连接被拒绝）→ LlmError::Network
        // 绑定空闲端口后立即释放，连接该端口会被拒绝
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);

        let base = format!("http://{addr}");
        let provider = OpenAiProvider::new(base, "sk-test", "gpt-4").expect("构造 provider");
        let result = provider.chat_stream(basic_req()).await;
        let Err(err) = result else {
            panic!("连接拒绝应返回错误，但 chat_stream 成功");
        };
        assert!(
            matches!(err, LlmError::Network(_)),
            "期望 Network 错误，得到 {err:?}"
        );
    }

    #[tokio::test]
    async fn chat_stream_sends_bearer_auth_and_model() {
        // 场景：验证请求体含 model 字段与 Bearer 鉴权头
        let server = MockServer::start().await;
        let expected_body = json!({"model": "gpt-4"});
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_partial_json(expected_body))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("data: [DONE]\n\n")
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "sk-test", "gpt-4").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        // 消费完流，触发请求验证（mock 未匹配会返回默认 404，导致请求失败）
        let _ = collect_deltas(stream).await;
    }

    // --- provider 基本方法 ---

    #[test]
    fn provider_id_and_capabilities() {
        let provider = OpenAiProvider::new("https://api.openai.com/v1", "sk-test", "gpt-4")
            .expect("构造 provider");
        assert_eq!(provider.id(), PROVIDER_ID);
        let caps = provider.capabilities();
        assert!(caps.supports_tool_call);
        assert!(caps.supports_streaming);
        assert!(caps.supports_json_mode);
        assert!(!caps.supports_vision);
    }

    #[tokio::test]
    async fn count_tokens_delegates_to_tokenizer() {
        let provider = OpenAiProvider::new("https://api.openai.com/v1", "sk-test", "gpt-4")
            .expect("构造 provider");
        let n = provider
            .count_tokens(&[Message::user_text("hello world")])
            .await;
        assert!(n > 0, "count_tokens 应返回正数: {n}");
    }

    #[test]
    fn debug_does_not_leak_api_key() {
        // C-04：Debug 输出脱敏 api_key（前 4 字符 + ***）
        let provider = OpenAiProvider::new("https://api.openai.com/v1", "sk-secret-12345", "gpt-4")
            .expect("构造 provider");
        let s = format!("{provider:?}");
        assert!(
            !s.contains("sk-secret"),
            "Debug 不应泄漏 api_key 前缀（resolver 隐藏）: {s}"
        );
        assert!(
            !s.contains("secret-12345"),
            "Debug 不应泄漏完整 api_key: {s}"
        );
    }

    // --- chat_stream 补充 ---

    #[tokio::test]
    async fn chat_stream_500_returns_server_error() {
        // 场景：HTTP 500 服务端错误 → LlmError::Server
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "sk-test", "gpt-4").expect("构造 provider");
        let result = provider.chat_stream(basic_req()).await;
        let Err(err) = result else {
            panic!("500 应返回错误，但 chat_stream 成功");
        };
        match err {
            LlmError::Server { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body, "internal error");
            }
            other => panic!("期望 Server 错误，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_invalid_json_returns_parse_error() {
        // 场景：SSE data 为非法 JSON → 流中返回 LlmError::Parse
        let server = MockServer::start().await;
        let sse_body = "data: not valid json\n\ndata: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "sk-test", "gpt-4").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let mut s = stream;
        let mut found_parse_error = false;
        while let Some(item) = s.next().await {
            if let Err(LlmError::Parse(_)) = item {
                found_parse_error = true;
                break;
            }
        }
        assert!(found_parse_error, "流中应包含 Parse 错误");
    }

    #[tokio::test]
    async fn chat_stream_multiple_tool_calls_in_single_chunk() {
        // 场景：单个 SSE chunk 的 delta.tool_calls 含多个 tool_call → 展开为多个 Delta::ToolCall
        let server = MockServer::start().await;
        let chunk = json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "call_1", "function": {"name": "fs.read", "arguments": "{}"}},
            {"index": 1, "id": "call_2", "function": {"name": "fs.write", "arguments": "{}"}}
        ]}}]});
        let sse_body = format!("{}data: [DONE]\n\n", sse_event(&chunk));
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(server.uri(), "sk-test", "gpt-4").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        assert_eq!(deltas.len(), 2);
        match &deltas[0] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 0);
                assert_eq!(tc.id.as_deref(), Some("call_1"));
                assert_eq!(tc.name.as_deref(), Some("fs.read"));
            }
            other => panic!("期望 ToolCall[0]，得到 {other:?}"),
        }
        match &deltas[1] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 1);
                assert_eq!(tc.id.as_deref(), Some("call_2"));
                assert_eq!(tc.name.as_deref(), Some("fs.write"));
            }
            other => panic!("期望 ToolCall[1]，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_trailing_slash_in_api_base_normalized() {
        // 场景：api_base 含尾部 / → URL 拼接时 trim_end_matches('/') 去重
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("data: [DONE]\n\n")
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let base = format!("{}/", server.uri());
        let provider = OpenAiProvider::new(base, "sk-test", "gpt-4").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        assert!(deltas.is_empty(), "expected empty: deltas");
    }

    // --- auth_headers 补充 ---

    #[test]
    fn auth_headers_invalid_api_key_returns_network_error() {
        // 包含换行符的 api_key 无法构造 HeaderValue → LlmError::Network
        let provider = OpenAiProvider::new("https://api.openai.com/v1", "sk-bad\nkey", "gpt-4")
            .expect("构造 provider");
        let result = provider.auth_headers();
        let Err(err) = result else {
            panic!("非法 api_key 应返回错误");
        };
        assert!(
            matches!(err, LlmError::Network(_)),
            "期望 Network 错误，得到 {err:?}"
        );
    }

    // --- parse_chunk 补充 ---

    #[test]
    fn parse_chunk_combined_content_tool_calls_finish_and_usage() {
        // 单个 chunk 同时含 content + tool_calls + finish_reason + usage
        let chunk = json!({
            "choices": [{
                "delta": {
                    "content": "thinking",
                    "tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "noop"}}]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        });
        let deltas = parse_chunk(&chunk);
        // 1 text + 1 tool_call + 1 stop + 1 usage = 4
        assert_eq!(deltas.len(), 4);
        assert!(matches!(&deltas[0], Delta::Text(t) if t == "thinking"));
        assert!(matches!(&deltas[1], Delta::ToolCall(tc) if tc.index == 0));
        assert!(matches!(&deltas[2], Delta::Stop(StopReason::ToolUse)));
        assert!(matches!(&deltas[3], Delta::Usage(_)));
    }

    // --- parse_usage 补充 ---

    #[test]
    fn parse_chunk_reasoning_content_deepseek() {
        // DeepSeek：delta.reasoning_content 与正文分离下发
        let chunk = json!({
            "choices": [{"delta": {"reasoning_content": "逐步分析中"}}]
        });
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Reasoning(t) if t == "逐步分析中"));
    }

    #[test]
    fn parse_chunk_reasoning_openai_o_series() {
        // OpenAI o 系列：delta.reasoning
        let chunk = json!({
            "choices": [{"delta": {"reasoning": "let me think"}}]
        });
        let deltas = parse_chunk(&chunk);
        assert!(matches!(&deltas[0], Delta::Reasoning(t) if t == "let me think"));
    }

    #[test]
    fn parse_chunk_reasoning_prioritizes_reasoning_content() {
        // 两者同时出现时以 reasoning_content 为准（DeepSeek 风格优先）
        let chunk = json!({
            "choices": [{"delta": {"reasoning": "o-style", "reasoning_content": "ds-style"}}]
        });
        let deltas = parse_chunk(&chunk);
        assert!(matches!(&deltas[0], Delta::Reasoning(t) if t == "ds-style"));
    }

    #[test]
    fn parse_usage_with_cached_tokens() {
        let u = parse_usage(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 30}
        }));
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cache_read, Some(30));
        assert!(u.cache_write.is_none(), "OpenAI 不返回 cache_write");
    }

    // --- extract_text / tool_content_to_string ---

    #[test]
    fn extract_text_with_tool_result() {
        let blocks = vec![ContentBlock::ToolResult {
            call_id: "call_1".to_string(),
            content: ToolContent::Text("result text".to_string()),
            is_error: false,
            metadata: minicoding_core::model::ToolResultMeta::default(),
        }];
        assert_eq!(extract_text(&blocks), "result text");
    }

    #[test]
    fn extract_text_joins_multiple_blocks() {
        let blocks = vec![
            ContentBlock::Text {
                text: "line1".into(),
            },
            ContentBlock::Text {
                text: "line2".into(),
            },
        ];
        assert_eq!(extract_text(&blocks), "line1\nline2");
    }

    #[test]
    fn tool_content_to_string_json_variant() {
        let s = tool_content_to_string(&ToolContent::Json(json!({"key": "val"})));
        assert!(s.contains("\"key\""));
        assert!(s.contains("val"));
    }

    #[test]
    fn tool_content_to_string_image_returns_empty() {
        let s = tool_content_to_string(&ToolContent::Image {
            mime: "image/png".to_string(),
            data: vec![1, 2, 3],
        });
        assert!(s.is_empty(), "expected empty: s");
    }

    #[test]
    fn tool_content_to_string_mixed_joins_parts() {
        let s = tool_content_to_string(&ToolContent::Mixed(vec![
            ToolContent::Text("part1".to_string()),
            ToolContent::Text("part2".to_string()),
        ]));
        assert_eq!(s, "part1\npart2");
    }

    // --- role_str ---

    #[test]
    fn role_str_all_variants() {
        assert_eq!(role_str(&Role::System), "system");
        assert_eq!(role_str(&Role::User), "user");
        assert_eq!(role_str(&Role::Assistant), "assistant");
        assert_eq!(role_str(&Role::Tool), "tool");
    }
}
