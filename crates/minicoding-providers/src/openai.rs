//! `OpenAI` 兼容 provider（支持 `OpenAI` / Azure `OpenAI` / Ollama 等 `OpenAI` 风格 API）。
//!
//! 通过 `reqwest` 发起 POST `{api_base}/chat/completions`，`stream: true`，按 SSE
//! 协议解析响应，转换为 [`Delta`]。HTTP 状态码映射到 [`LlmError`]：
//! 429 → `RateLimited`（携带 `Retry-After`），5xx → `Server`，其它 4xx → `Client`。

use futures::stream::{self, BoxStream, Stream, StreamExt};
use minicoding_core::model::{ContentBlock, LlmError, Message, Role, StopReason, ToolContent};
use minicoding_core::provider::{
    BoxFuture, Capabilities, ChatRequest, Delta, LlmProvider, Tokenizer, ToolCallDelta, Usage,
};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::{debug, warn};

use crate::tokenizer::TiktokenTokenizer;

/// Provider 标识。
pub const PROVIDER_ID: &str = "openai";

/// `OpenAI` 兼容 LLM provider。
///
/// 构造后通过 `Arc<dyn LlmProvider>` 注入 Runtime。所有方法返回 `BoxFuture` /
/// `BoxStream`，保证 `dyn` 兼容（见 `core::provider::trait`）。
pub struct OpenAiProvider {
    api_base: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
    tokenizer: Arc<TiktokenTokenizer>,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("api_base", &self.api_base)
            .field("model", &self.model)
            .field("tokenizer", &self.tokenizer.kind())
            // 不输出 api_key（C-04：日志脱敏，前 4 字符 + ***）
            .field("api_key", &mask_key(&self.api_key))
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// 构造 provider。
    ///
    /// `api_base` 形如 `https://api.openai.com/v1`，无需尾部 `/`；`model` 决定分词器与
    /// 请求中的 `model` 字段。
    ///
    /// # Errors
    /// - `reqwest::Client` 初始化失败 → [`LlmError::Network`]
    /// - 分词器加载失败 → [`LlmError::Parse`]
    pub fn new(
        api_base: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        let model_str = model.into();
        let tokenizer = TiktokenTokenizer::new_for_model(&model_str).map_err(LlmError::Parse)?;
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;
        Ok(Self {
            api_base: api_base.into(),
            api_key: api_key.into(),
            model: model_str,
            client,
            tokenizer: Arc::new(tokenizer),
        })
    }

    /// 构造 POST 请求体（OpenAI chat completions 格式，`stream: true`）。
    fn build_request_body(&self, req: &ChatRequest) -> Value {
        let mut messages = Vec::with_capacity(req.messages.len() + 1);
        if !req.system.is_empty() {
            messages.push(json!({"role": "system", "content": req.system}));
        }
        for m in &req.messages {
            messages.push(message_to_openai(m));
        }

        let mut body = json!({
            "model": self.model,
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

    /// 构造鉴权 headers。
    fn auth_headers(&self) -> Result<HeaderMap, LlmError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let bearer = format!("Bearer {}", self.api_key);
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
        PROVIDER_ID
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

            let resp = self
                .client
                .post(&url)
                .headers(self.auth_headers()?)
                .json(&body)
                .send()
                .await
                .map_err(|e| LlmError::Network(e.to_string()))?;

            let status = resp.status();
            if !status.is_success() {
                let retry_after_ms = retry_after_ms(resp.headers());
                let body_text = resp.text().await.unwrap_or_default();
                return Err(map_status_error(status.as_u16(), body_text, retry_after_ms));
            }

            // bytes_stream() 返回的流是 'static（消费 response），转换为 Vec<u8> 避免
            // 引入 bytes 依赖。
            let byte_stream = resp.bytes_stream().map(|r| r.map(|b| b.to_vec())).boxed();
            let sse = SseStream::new(byte_stream);
            let delta_stream = sse
                .flat_map(|ev| {
                    let items: Vec<Result<Delta, LlmError>> = match ev {
                        Ok(json) => parse_chunk(&json).into_iter().map(Ok).collect(),
                        Err(e) => vec![Err(e)],
                    };
                    stream::iter(items)
                })
                .boxed();

            Ok(delta_stream)
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
    if m.role == Role::Tool {
        let mut obj = serde_json::Map::new();
        obj.insert("role".to_string(), Value::String(role.to_string()));
        if let Some(call_id) = &m.tool_call_id {
            obj.insert("tool_call_id".to_string(), Value::String(call_id.clone()));
        }
        obj.insert("content".to_string(), Value::String(text));
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

/// 从 `ContentBlock` 列表提取文本（含 `ToolResult` 内容；忽略 `Image` 与冗余 `ToolUse`）。
fn extract_text(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => parts.push(t.clone()),
            ContentBlock::ToolResult { content, .. } => {
                parts.push(tool_content_to_string(content));
            }
            ContentBlock::ToolUse(_) | ContentBlock::Image { .. } => {}
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

    if let Some(choices) = chunk.get("choices").and_then(Value::as_array) {
        if let Some(choice) = choices.first() {
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content").and_then(Value::as_str) {
                    if !content.is_empty() {
                        deltas.push(Delta::Text(content.to_string()));
                    }
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
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if !reason.is_empty() {
                    deltas.push(Delta::Stop(map_stop_reason(reason)));
                }
            }
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

/// API key 脱敏（前 4 字符 + `***`），用于日志输出（C-04）。
fn mask_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 4 {
        "***".to_string()
    } else {
        let head: String = key.chars().take(4).collect();
        format!("{head}***")
    }
}

/// SSE 解析流状态。
struct SseStream {
    /// 底层字节流（已转为 `Vec<u8>`，避免 `bytes` 依赖）。
    inner: Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>,
    /// 累积未消费的字节（按 UTF-8 lossy 解码）。
    buffer: String,
    /// 底层流是否已结束。
    done: bool,
}

impl SseStream {
    /// 构造 SSE 解析器。
    fn new(inner: Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>) -> Self {
        Self {
            inner,
            buffer: String::new(),
            done: false,
        }
    }

    /// 尝试从 buffer 取出一个完整事件（以 `\n\n` 分隔）。
    fn take_event(&mut self) -> Option<String> {
        let pos = self.buffer.find("\n\n")?;
        let event: String = self.buffer.drain(..pos + 2).collect();
        Some(event)
    }

    /// 解析单个 SSE 事件，返回 `Ok(Some(json))` / `Ok(None)`（无 data 行或 `[DONE]`）。
    /// 返回 `Err` 表示 JSON 解析失败。
    fn parse_event(event: &str) -> Result<Option<Value>, LlmError> {
        let mut data_lines: Vec<&str> = Vec::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                let trimmed = rest.strip_prefix(' ').unwrap_or(rest).trim();
                data_lines.push(trimmed);
            }
        }
        if data_lines.is_empty() {
            return Ok(None);
        }
        let data = data_lines.join("\n");
        if data == "[DONE]" {
            return Ok(None);
        }
        serde_json::from_str(&data)
            .map(Some)
            .map_err(|e| LlmError::Parse(e.to_string()))
    }
}

impl Stream for SseStream {
    type Item = Result<Value, LlmError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // 1) 优先消费已缓冲的完整事件
            if let Some(event) = self.take_event() {
                let was_done = event.contains("[DONE]");
                match Self::parse_event(&event) {
                    Ok(Some(json)) => return Poll::Ready(Some(Ok(json))),
                    Ok(None) => {
                        if was_done {
                            return Poll::Ready(None);
                        }
                        continue;
                    }
                    Err(e) => {
                        warn!(target: "minicoding::provider::openai", error = %e, "sse parse failed");
                        return Poll::Ready(Some(Err(e)));
                    }
                }
            }

            // 2) buffer 不完整时尝试 flush 末尾残留（流结束后）
            if self.done {
                if !self.buffer.is_empty() {
                    let event = std::mem::take(&mut self.buffer);
                    match Self::parse_event(&event) {
                        Ok(Some(json)) => return Poll::Ready(Some(Ok(json))),
                        Ok(None) => return Poll::Ready(None),
                        Err(e) => return Poll::Ready(Some(Err(e))),
                    }
                }
                return Poll::Ready(None);
            }

            // 3) 拉取底层流
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(LlmError::Network(e.to_string()))));
                }
                Poll::Ready(None) => {
                    self.done = true;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
