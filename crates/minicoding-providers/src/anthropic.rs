//! Anthropic provider（`/v1/messages` 事件流，T-M6-1，`features.md` L-02/L-07）。
//!
//! 通过 `reqwest` 发起 POST `{api_base}/v1/messages`，`stream: true`，按 SSE 协议解析
//! 响应。与 `OpenAI` 的关键差异：
//!
//! - **system 分离**：system prompt 是顶层 `system` 字段，不放入 `messages`；
//! - **鉴权头**：`x-api-key` + `anthropic-version: 2023-06-01`（非 `Bearer`）；
//! - **事件流**：按 JSON `type` 字段分派（`content_block_start`/`content_block_delta`/
//!   `message_delta`/`message_stop`），非 `choices[].delta`；
//! - **工具调用**：`tool_use` content block + `input_json_delta` 分片（index 对齐）；
//! - **Vision**：`image` content block（base64 `source`），`supports_vision: true`。
//!
//! HTTP 状态码映射同 `OpenAI`：429 → `RateLimited`（携带 `Retry-After`），5xx → `Server`，
//! 其它 4xx → `Client`。重试由 `RetryProvider` 装饰（T-M6-3）。

use futures::stream::{self, StreamExt};
use minicoding_core::model::{ContentBlock, LlmError, Message, Role, StopReason, ToolContent};
use minicoding_core::provider::{
    BoxFuture, BoxStream, Capabilities, ChatRequest, Delta, LlmProvider, Tokenizer, ToolCallDelta,
    Usage,
};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::debug;

/// Provider 标识。
pub const PROVIDER_ID: &str = "anthropic";

/// Anthropic API 版本头（见 `design.md` §4.2）。
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic LLM provider。
///
/// 构造后通过 `Arc<dyn LlmProvider>` 注入 Runtime。token 计数为近似（Anthropic 未公开
/// 分词器，按 4 字符 ≈ 1 token 估算，`design.md` §4.4）。
pub struct AnthropicProvider {
    api_base: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
    tokenizer: Arc<ApproxTokenizer>,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("api_base", &self.api_base)
            .field("model", &self.model)
            // 不输出 api_key（C-04：日志脱敏）
            .field("api_key", &crate::common::mask_key(&self.api_key))
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// 构造 provider。
    ///
    /// `api_base` 形如 `https://api.anthropic.com`（无需尾部 `/`，构造时拼 `/v1/messages`）。
    ///
    /// # Errors
    /// `reqwest::Client` 初始化失败 → [`LlmError::Network`]
    pub fn new(
        api_base: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;
        Ok(Self {
            api_base: api_base.into(),
            api_key: api_key.into(),
            model: model.into(),
            client,
            tokenizer: Arc::new(ApproxTokenizer),
        })
    }

    /// 构造 POST 请求体（Anthropic messages 格式，`stream: true`）。
    /// system prompt 放顶层 `system` 字段，不进 messages（见 `design.md` §4.2）。
    fn build_request_body(&self, req: &ChatRequest) -> Value {
        let messages: Vec<Value> = req.messages.iter().map(message_to_anthropic).collect();

        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "max_tokens": req.params.max_output_tokens.unwrap_or(4_096),
        });

        // system prompt 顶层分离（Anthropic 不接受 messages 里的 system role）
        if !req.system.is_empty() {
            body["system"] = json!(req.system);
        }

        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
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
        if !req.params.stop.is_empty() {
            body["stop_sequences"] = json!(req.params.stop);
        }
        body
    }

    /// 构造鉴权 headers（`x-api-key` + `anthropic-version`）。
    fn auth_headers(&self) -> Result<HeaderMap, LlmError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&self.api_key)
                .map_err(|e| LlmError::Network(format!("invalid api key: {e}")))?,
        );
        Ok(headers)
    }
}

impl LlmProvider for AnthropicProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tool_call: true,
            supports_vision: true,
            supports_streaming: true,
            supports_json_mode: false,
            // Claude 3.5 Sonnet 200K 上下文窗口
            context_window: 200_000,
            max_output: 8_192,
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
            let url = format!("{}/v1/messages", self.api_base.trim_end_matches('/'));

            debug!(
                target: "minicoding::provider::anthropic",
                model = %self.model, url = %url, "POST v1/messages stream"
            );

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

            // SSE 解析复用 common::sse，data payload 为字符串，此处解析 JSON 后按 `type` 分派。
            // `Box::pin`（非 `.boxed()`）保留 `Send` 约束（见 openai.rs 同样注释）。
            let sse = crate::common::sse::from_response(resp);
            let delta_stream = sse.flat_map(|ev| {
                let items: Vec<Result<Delta, LlmError>> = match ev {
                    Ok(data) => match serde_json::from_str::<Value>(&data) {
                        Ok(json) => parse_event(&json).into_iter().map(Ok).collect(),
                        Err(e) => vec![Err(LlmError::Parse(e.to_string()))],
                    },
                    Err(e) => vec![Err(e)],
                };
                stream::iter(items)
            });

            Ok(Box::pin(delta_stream) as BoxStream<'static, _>)
        })
    }

    fn count_tokens(&self, messages: &[Message]) -> BoxFuture<'_, usize> {
        let n = self.tokenizer.count_messages(messages);
        Box::pin(async move { n })
    }
}

/// 将 [`Message`] 映射到 Anthropic messages wire format。
///
/// 关键差异：tool 结果在 Anthropic 中是 **user** 角色的 `tool_result` content block
/// （非 `role: tool`）；assistant 工具调用是 `tool_use` content block。
fn message_to_anthropic(m: &Message) -> Value {
    let role = match m.role {
        // Anthropic 无 tool role：tool 结果作为 user 消息的 tool_result block
        Role::System | Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
    };

    // tool 结果消息：user + tool_result content block
    if m.role == Role::Tool {
        let call_id = m.tool_call_id.clone().unwrap_or_default();
        let text = extract_text(&m.content);
        return json!({
            "role": role,
            "content": [{"type": "tool_result", "tool_use_id": call_id, "content": text}],
        });
    }

    // assistant + tool_calls：assistant + tool_use content blocks（+ 可选 text）
    if m.role == Role::Assistant && !m.tool_calls.is_empty() {
        let mut blocks: Vec<Value> = Vec::new();
        let text = extract_text(&m.content);
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
        for tc in &m.tool_calls {
            blocks.push(json!({
                "type": "tool_use",
                "id": tc.id,
                "name": tc.name,
                "input": tc.input,
            }));
        }
        return json!({"role": role, "content": blocks});
    }

    // 默认 user/assistant：content blocks（含 image，Vision L-07）
    let blocks = content_to_blocks(&m.content);
    json!({"role": role, "content": blocks})
}

/// 将 [`ContentBlock`] 列表转为 Anthropic content blocks（含 image，Vision L-07）。
fn content_to_blocks(blocks: &[ContentBlock]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                out.push(json!({"type": "text", "text": text}));
            }
            ContentBlock::Image { mime, data } => {
                // Anthropic image：base64 source（Vision L-07，design.md §4.5）
                out.push(json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": mime, "data": data},
                }));
            }
            // ToolUse/ToolResult 在上面分支单独处理，此处忽略
            ContentBlock::ToolUse(_) | ContentBlock::ToolResult { .. } => {}
        }
    }
    if out.is_empty() {
        out.push(json!({"type": "text", "text": ""}));
    }
    out
}

/// 从 `ContentBlock` 列表提取文本（含 `ToolResult` 内容；忽略 `Image`/`ToolUse`）。
fn extract_text(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: t } => parts.push(t.clone()),
            ContentBlock::ToolResult { content, .. } => {
                parts.push(tool_content_to_string(content));
            }
            ContentBlock::ToolUse(_) | ContentBlock::Image { .. } => {}
        }
    }
    parts.join("\n")
}

/// 将 [`ToolContent`] 序列化为字符串。
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

/// HTTP 状态码 → [`LlmError`]（与 `OpenAI` 一致，便于 `RetryProvider` 统一处理）。
fn map_status_error(status: u16, body: String, retry_after_ms: Option<u64>) -> LlmError {
    match status {
        429 => LlmError::RateLimited { retry_after_ms },
        s if (500..600).contains(&s) => LlmError::Server { status: s, body },
        s => LlmError::Client { status: s, body },
    }
}

/// 从 `Retry-After` header 解析重试毫秒数（仅秒数形式）。
fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000))
}

/// 解析 Anthropic 事件 JSON，按 `type` 字段分派，返回零到多个 [`Delta`]。
///
/// 事件类型（见 `design.md` §4.3）：
/// - `message_start`：含 `usage.input_tokens` → `Delta::Usage`
/// - `content_block_start`：`tool_use` block → `Delta::ToolCall`（id/name）
/// - `content_block_delta`：`text_delta` → `Delta::Text`；`input_json_delta` → `Delta::ToolCall`（args 分片）
/// - `message_delta`：`stop_reason` → `Delta::Stop`；`usage.output_tokens` → `Delta::Usage`
/// - `message_stop`/`ping`：跳过（流自然结束）
fn parse_event(event: &Value) -> Vec<Delta> {
    let mut deltas = Vec::new();
    let ty = event.get("type").and_then(Value::as_str).unwrap_or("");

    match ty {
        "message_start" => {
            if let Some(usage) = event.get("message").and_then(|m| m.get("usage")) {
                deltas.push(Delta::Usage(parse_usage(usage)));
            }
        }
        "content_block_start" => {
            // tool_use block 开始：产出 id/name（args 后续由 input_json_delta 分片）
            if let Some(block) = event.get("content_block")
                && block.get("type").and_then(Value::as_str) == Some("tool_use")
            {
                let index = u32_from_json(event.get("index"));
                let id = block.get("id").and_then(Value::as_str).map(String::from);
                let name = block.get("name").and_then(Value::as_str).map(String::from);
                deltas.push(Delta::ToolCall(ToolCallDelta {
                    index,
                    id,
                    name,
                    args_chunk: None,
                }));
            }
        }
        "content_block_delta" => {
            let index = u32_from_json(event.get("index"));
            if let Some(delta) = event.get("delta") {
                let dtype = delta.get("type").and_then(Value::as_str).unwrap_or("");
                match dtype {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            deltas.push(Delta::Text(text.to_string()));
                        }
                    }
                    "input_json_delta" => {
                        // 工具调用入参分片（partial_json），与 OpenAI 的 args_chunk 对齐
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .map(String::from);
                        deltas.push(Delta::ToolCall(ToolCallDelta {
                            index,
                            id: None,
                            name: None,
                            args_chunk: partial,
                        }));
                    }
                    _ => {}
                }
            }
        }
        "message_delta" => {
            // stop_reason + 累计 output_tokens
            if let Some(d) = event.get("delta")
                && let Some(reason) = d.get("stop_reason").and_then(Value::as_str)
                && !reason.is_empty()
            {
                deltas.push(Delta::Stop(map_stop_reason(reason)));
            }
            if let Some(usage) = event.get("usage") {
                deltas.push(Delta::Usage(parse_usage(usage)));
            }
        }
        // message_stop / ping / content_block_stop：不产出 delta
        _ => {}
    }

    deltas
}

/// 解析 Anthropic `usage` 对象为 [`Usage`]。
///
/// `message_start` 含 `input_tokens`/`output_tokens`（初始）；`message_delta` 含
/// 累计 `output_tokens`。cache 字段 Anthropic 用 `cache_creation_input_tokens`/
/// `cache_read_input_tokens`。
fn parse_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usize_from_json(usage.get("input_tokens")),
        output_tokens: usize_from_json(usage.get("output_tokens")),
        cache_read: usize_from_option_opt(usage.get("cache_read_input_tokens")),
        cache_write: usize_from_option_opt(usage.get("cache_creation_input_tokens")),
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

/// 从 `&Value` 取 `usize`，缺失返回 `None`（用于可选 cache 字段）。
fn usize_from_option_opt(v: Option<&Value>) -> Option<usize> {
    v.and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

/// Anthropic `stop_reason` → [`StopReason`]。
fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        // stop_sequence 与未知值统一映射为 Stopped（Anthropic 文档未定义其它取值）
        _ => StopReason::Stopped,
    }
}

/// 近似分词器（Anthropic 未公开分词器，按 4 字符 ≈ 1 token 估算，`design.md` §4.4）。
#[derive(Debug, Default)]
pub struct ApproxTokenizer;

impl Tokenizer for ApproxTokenizer {
    fn count(&self, text: &str) -> usize {
        // 4 字符 ≈ 1 token（英文经验值；中文偏低估但不影响熔断判定）
        text.chars().count().div_ceil(4)
    }
    fn count_messages(&self, msgs: &[Message]) -> usize {
        msgs.iter()
            .map(|m| {
                // 每条消息加 4 token overhead（角色标记等），与 tiktoken 习惯对齐
                4 + extract_text(&m.content).chars().count().div_ceil(4)
            })
            .sum()
    }
    fn id(&self) -> &'static str {
        "anthropic-approx"
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn parse_message_start_emits_usage() {
        let ev = json!({
            "type": "message_start",
            "message": {"usage": {"input_tokens": 10, "output_tokens": 1}}
        });
        let deltas = parse_event(&ev);
        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            Delta::Usage(u) => {
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 1);
            }
            other => panic!("期望 Usage，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_text_delta_emits_text() {
        let ev = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"}
        });
        let deltas = parse_event(&ev);
        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            Delta::Text(t) => assert_eq!(t, "Hello"),
            other => panic!("期望 Text，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_tool_use_start_emits_toolcall_id_name() {
        let ev = json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_01", "name": "fs.read", "input": {}}
        });
        let deltas = parse_event(&ev);
        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 1);
                assert_eq!(tc.id.as_deref(), Some("toolu_01"));
                assert_eq!(tc.name.as_deref(), Some("fs.read"));
                assert!(tc.args_chunk.is_none());
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_input_json_delta_emits_args_chunk() {
        let ev = json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}
        });
        let deltas = parse_event(&ev);
        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 1);
                assert_eq!(tc.args_chunk.as_deref(), Some("{\"path\":"));
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_message_delta_emits_stop_and_usage() {
        let ev = json!({
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"output_tokens": 42}
        });
        let deltas = parse_event(&ev);
        assert_eq!(deltas.len(), 2);
        assert!(matches!(deltas[0], Delta::Stop(StopReason::ToolUse)));
        assert!(matches!(deltas[1], Delta::Usage(_)));
    }

    #[test]
    fn parse_message_stop_and_ping_skipped() {
        assert!(parse_event(&json!({"type": "message_stop"})).is_empty());
        assert!(parse_event(&json!({"type": "ping"})).is_empty());
        assert!(parse_event(&json!({"type": "content_block_stop", "index": 0})).is_empty());
    }

    #[test]
    fn map_stop_reason_variants() {
        assert_eq!(map_stop_reason("end_turn"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("stop_sequence"), StopReason::Stopped);
        assert_eq!(map_stop_reason("unknown"), StopReason::Stopped);
    }

    #[test]
    fn tool_result_message_maps_to_user_tool_result() {
        let mut msg = Message::user_text("result");
        msg.role = Role::Tool;
        msg.tool_call_id = Some("toolu_01".into());
        let v = message_to_anthropic(&msg);
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"][0]["type"], "tool_result");
        assert_eq!(v["content"][0]["tool_use_id"], "toolu_01");
        assert_eq!(v["content"][0]["content"], "result");
    }

    #[test]
    fn assistant_with_tool_calls_emits_tool_use_blocks() {
        let mut msg = Message::assistant_text("thinking");
        msg.tool_calls = vec![minicoding_core::model::ToolCall {
            id: "toolu_02".into(),
            name: "fs.read".into(),
            input: json!({"path": "/tmp"}),
        }];
        let v = message_to_anthropic(&msg);
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "thinking");
        assert_eq!(v["content"][1]["type"], "tool_use");
        assert_eq!(v["content"][1]["id"], "toolu_02");
        assert_eq!(v["content"][1]["input"]["path"], "/tmp");
    }

    #[test]
    fn image_content_maps_to_base64_source() {
        let blocks = content_to_blocks(&[ContentBlock::Image {
            mime: "image/png".into(),
            data: "iVBOR...".into(),
        }]);
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["type"], "base64");
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[0]["source"]["data"], "iVBOR...");
    }

    #[test]
    fn build_request_body_separates_system() {
        let provider =
            AnthropicProvider::new("https://api.anthropic.com", "sk-test", "claude-3-5-sonnet")
                .expect("构造");
        let req = ChatRequest {
            system: "You are helpful.".into(),
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            params: minicoding_core::provider::GenerationParams {
                model: "claude-3-5-sonnet".into(),
                temperature: None,
                top_p: None,
                max_output_tokens: Some(1_024),
                stop: vec![],
                seed: None,
            },
        };
        let body = provider.build_request_body(&req);
        // system 顶层分离，不在 messages 里
        assert_eq!(body["system"], "You are helpful.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body["messages"][0].get("system").is_none());
        assert_eq!(body["max_tokens"], 1_024);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn approx_tokenizer_counts_chars_divided_by_4() {
        let tok = ApproxTokenizer;
        assert_eq!(tok.count("abcdefgh"), 2); // 8 字符 / 4 = 2
        assert_eq!(tok.count("abc"), 1); // 3 字符 div_ceil 4 = 1
        assert_eq!(tok.id(), "anthropic-approx");
    }
}
