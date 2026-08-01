//! Ollama provider（`/api/chat` NDJSON 流，T-M6-2，`features.md` L-03）。
//!
//! 通过 `reqwest` 发起 POST `{api_base}/api/chat`，`stream: true`，按 NDJSON 协议解析
//! 响应（每行一个 JSON 对象，以 `\n` 分隔）。与 `OpenAI`/`Anthropic` 的关键差异：
//!
//! - **无鉴权**：本地服务，默认 `http://localhost:11434`，无 `Authorization` 头（P-09）；
//! - **NDJSON 流**：每行一个 JSON 对象（非 SSE 事件），字段 `message.content` 为文本增量，
//!   `message.tool_calls` 为工具调用（一次性，非分片），`done: true` 标记结束；
//! - **system 角色**：Ollama 接受 `messages` 中的 `system` role（不分离）；
//! - **工具调用**：`tool_calls[].function.{name,arguments}`，`arguments` 为 JSON 对象
//!   （非字符串，与 `OpenAI` 不同），转换时需序列化为字符串以适配 `ToolCall::input`；
//! - **token 统计**：`done: true` 行携带 `prompt_eval_count`/`eval_count`（非流式 usage）。
//!
//! HTTP 状态码映射同 `OpenAI`：429 → `RateLimited`，5xx → `Server`，其它 4xx → `Client`。
//! 重试由 `RetryProvider` 装饰（T-M6-3）。

use futures::stream::{self, BoxStream, StreamExt};
use minicoding_core::model::{ContentBlock, LlmError, Message, Role, StopReason, ToolContent};
use minicoding_core::provider::{
    BoxFuture, Capabilities, ChatRequest, Delta, LlmProvider, Tokenizer, ToolCallDelta, Usage,
};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::debug;

use crate::tokenizer::TiktokenTokenizer;

/// Provider 标识。
pub const PROVIDER_ID: &str = "ollama";

/// 默认 API base（本地 Ollama 服务）。
pub const DEFAULT_API_BASE: &str = "http://localhost:11434";

/// Ollama LLM provider。
///
/// 构造后通过 `Arc<dyn LlmProvider>` 注入 Runtime。token 计数复用 `TiktokenTokenizer`
/// （Ollama 未提供分词器，本地模型多为 Llama 系列，`cl100k_base` 为合理近似）。
pub struct OllamaProvider {
    api_base: String,
    model: String,
    client: reqwest::Client,
    tokenizer: Arc<TiktokenTokenizer>,
}

impl std::fmt::Debug for OllamaProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaProvider")
            .field("api_base", &self.api_base)
            .field("model", &self.model)
            .field("tokenizer", &self.tokenizer.kind())
            .finish_non_exhaustive()
    }
}

impl OllamaProvider {
    /// 构造 provider。
    ///
    /// `api_base` 形如 `http://localhost:11434`（无需尾部 `/`）；`model` 决定请求中的
    /// `model` 字段与分词器选择。
    ///
    /// # Errors
    /// - `reqwest::Client` 初始化失败 → [`LlmError::Network`]
    /// - 分词器加载失败 → [`LlmError::Parse`]
    pub fn new(api_base: impl Into<String>, model: impl Into<String>) -> Result<Self, LlmError> {
        let model_str = model.into();
        let tokenizer = TiktokenTokenizer::new_for_model(&model_str).map_err(LlmError::Parse)?;
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;
        Ok(Self {
            api_base: api_base.into(),
            model: model_str,
            client,
            tokenizer: Arc::new(tokenizer),
        })
    }

    /// 构造 POST 请求体（Ollama chat 格式，`stream: true`）。
    fn build_request_body(&self, req: &ChatRequest) -> Value {
        let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if !req.system.is_empty() {
            messages.push(json!({"role": "system", "content": req.system}));
        }
        for m in &req.messages {
            messages.push(message_to_ollama(m));
        }

        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
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

        // Ollama 选项参数（temperature/top_p 等放 `options` 对象）
        let mut options = serde_json::Map::new();
        if let Some(t) = req.params.temperature {
            options.insert("temperature".to_string(), json!(t));
        }
        if let Some(t) = req.params.top_p {
            options.insert("top_p".to_string(), json!(t));
        }
        if let Some(m) = req.params.max_output_tokens {
            options.insert("num_predict".to_string(), json!(m));
        }
        if !req.params.stop.is_empty() {
            options.insert("stop".to_string(), json!(req.params.stop));
        }
        if let Some(seed) = req.params.seed {
            options.insert("seed".to_string(), json!(seed));
        }
        if !options.is_empty() {
            body["options"] = Value::Object(options);
        }
        body
    }

    /// 构造请求 headers（仅 `Content-Type`，Ollama 无鉴权）。
    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers
    }
}

impl LlmProvider for OllamaProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tool_call: true,
            // Ollama 多模态取决于模型（llava 等），此处保守 false，由调用方按模型判断
            supports_vision: false,
            supports_streaming: true,
            supports_json_mode: true,
            // 本地模型上下文窗口取决于模型配置，保守 8K（可通过 Modelfile 调整）
            context_window: 8_192,
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
            let url = format!("{}/api/chat", self.api_base.trim_end_matches('/'));

            debug!(
                target: "minicoding::provider::ollama",
                model = %self.model, url = %url, "POST api/chat stream"
            );

            let resp = self
                .client
                .post(&url)
                .headers(Self::headers())
                .json(&body)
                .send()
                .await
                .map_err(|e| LlmError::Network(e.to_string()))?;

            let status = resp.status();
            if !status.is_success() {
                let body_text = resp.text().await.unwrap_or_default();
                return Err(map_status_error(status.as_u16(), body_text));
            }

            // NDJSON 解析：每行一个 JSON 对象，按 `done` 字段判断结束
            let ndjson = crate::common::ndjson::from_response(resp);
            let delta_stream = ndjson
                .flat_map(|ev| {
                    let items: Vec<Result<Delta, LlmError>> = match ev {
                        Ok(line) => match serde_json::from_str::<Value>(&line) {
                            Ok(json) => parse_chunk(&json).into_iter().map(Ok).collect(),
                            Err(e) => vec![Err(LlmError::Parse(e.to_string()))],
                        },
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

/// 将 [`Message`] 映射到 Ollama chat wire format（与 `OpenAI` 类似，但 `arguments` 为对象）。
fn message_to_ollama(m: &Message) -> Value {
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

    // assistant + tool_calls：Ollama 的 arguments 是 JSON 对象（非字符串）
    if m.role == Role::Assistant && !m.tool_calls.is_empty() {
        let tool_calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|tc| {
                json!({
                    "function": {
                        "name": tc.name,
                        "arguments": tc.input,
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

/// 返回 role 的小写字符串表示。
fn role_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// HTTP 状态码 → [`LlmError`]（Ollama 不返回 `Retry-After`，429 仍映射为 `RateLimited`）。
fn map_status_error(status: u16, body: String) -> LlmError {
    match status {
        429 => LlmError::RateLimited {
            retry_after_ms: None,
        },
        s if (500..600).contains(&s) => LlmError::Server { status: s, body },
        s => LlmError::Client { status: s, body },
    }
}

/// 解析 Ollama NDJSON 行，转换为零到多个 [`Delta`]。
///
/// 每行结构：
/// - 流中：`{"message": {"role": "assistant", "content": "...", "tool_calls": [...]}, "done": false}`
/// - 结束：`{"done": true, "prompt_eval_count": N, "eval_count": M, ...}`
///
/// 工具调用一次性出现（非分片），统一映射为 `Delta::ToolCall`（`index=0`，`args_chunk` 为完整 JSON）。
fn parse_chunk(chunk: &Value) -> Vec<Delta> {
    let mut deltas = Vec::new();

    // 文本增量
    if let Some(message) = chunk.get("message") {
        if let Some(content) = message.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            deltas.push(Delta::Text(content.to_string()));
        }

        // 工具调用（一次性，非分片）
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for (i, tc) in tool_calls.iter().enumerate() {
                let function = tc.get("function");
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from);
                // Ollama 的 arguments 是 JSON 对象，序列化为字符串以适配 ToolCallDelta
                let args_chunk = function
                    .and_then(|f| f.get("arguments"))
                    .map(ToString::to_string);
                let id = function
                    .and_then(|f| f.get("id"))
                    .and_then(Value::as_str)
                    .map(String::from);
                deltas.push(Delta::ToolCall(ToolCallDelta {
                    index: u32::try_from(i).unwrap_or(0),
                    id,
                    name,
                    args_chunk,
                }));
            }
        }
    }

    // 结束行：done=true 携带 token 统计
    if chunk.get("done").and_then(Value::as_bool) == Some(true) {
        // Ollama 的 stop_reason 字段（部分版本支持）
        let stop_reason = chunk
            .get("done_reason")
            .and_then(Value::as_str)
            .map_or(StopReason::EndTurn, map_stop_reason);
        deltas.push(Delta::Stop(stop_reason));

        // token 统计（prompt_eval_count = input, eval_count = output）
        let input = usize_from_json(chunk.get("prompt_eval_count"));
        let output = usize_from_json(chunk.get("eval_count"));
        if input > 0 || output > 0 {
            deltas.push(Delta::Usage(Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read: None,
                cache_write: None,
            }));
        }
    }

    deltas
}

/// Ollama `done_reason` → [`StopReason`]。
fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "length" => StopReason::MaxTokens,
        "tools" => StopReason::ToolUse,
        // stop/load 与未知值统一映射为 EndTurn（Ollama 文档未定义其它取值）
        _ => StopReason::EndTurn,
    }
}

/// 从 JSON number 取 `usize`，缺失时返回 0。
fn usize_from_json(v: Option<&Value>) -> usize {
    v.and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn parse_text_delta() {
        let chunk = json!({
            "model": "llama3",
            "message": {"role": "assistant", "content": "hello"},
            "done": false
        });
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Text(t) if t == "hello"));
    }

    #[test]
    fn parse_empty_content_skipped() {
        let chunk = json!({
            "message": {"role": "assistant", "content": ""},
            "done": false
        });
        let deltas = parse_chunk(&chunk);
        assert!(deltas.is_empty());
    }

    #[test]
    fn parse_tool_call_one_shot() {
        let chunk = json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "fs.read",
                        "arguments": {"path": "/tmp"}
                    }
                }]
            },
            "done": false
        });
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 1);
        match &deltas[0] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 0);
                assert_eq!(tc.name.as_deref(), Some("fs.read"));
                // arguments 序列化为 JSON 字符串
                assert!(tc.args_chunk.as_ref().is_some_and(|s| s.contains("/tmp")));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_done_with_usage() {
        let chunk = json!({
            "done": true,
            "done_reason": "stop",
            "prompt_eval_count": 42,
            "eval_count": 10
        });
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 2);
        assert!(matches!(&deltas[0], Delta::Stop(StopReason::EndTurn)));
        match &deltas[1] {
            Delta::Usage(u) => {
                assert_eq!(u.input_tokens, 42);
                assert_eq!(u.output_tokens, 10);
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_done_without_usage() {
        // 部分模型不返回 token 统计
        let chunk = json!({"done": true});
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Stop(_)));
    }

    #[test]
    fn map_stop_reason_variants() {
        assert_eq!(map_stop_reason("stop"), StopReason::EndTurn);
        assert_eq!(map_stop_reason("length"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("tools"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("unknown"), StopReason::EndTurn);
    }

    #[test]
    fn tool_result_message_maps_to_tool_role() {
        let mut msg = Message::user_text("result");
        msg.role = Role::Tool;
        msg.tool_call_id = Some("call_01".into());
        let v = message_to_ollama(&msg);
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_01");
        assert_eq!(v["content"], "result");
    }

    #[test]
    fn assistant_with_tool_calls_emits_function_array() {
        let mut msg = Message::assistant_text("thinking");
        msg.tool_calls = vec![minicoding_core::model::ToolCall {
            id: "call_02".into(),
            name: "fs.read".into(),
            input: json!({"path": "/tmp"}),
        }];
        let v = message_to_ollama(&msg);
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"], "thinking");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "fs.read");
        // arguments 是 JSON 对象（非字符串）
        assert_eq!(v["tool_calls"][0]["function"]["arguments"]["path"], "/tmp");
    }

    #[test]
    fn system_message_kept_in_messages() {
        let msg = Message::system_text("you are helpful");
        let v = message_to_ollama(&msg);
        assert_eq!(v["role"], "system");
        assert_eq!(v["content"], "you are helpful");
    }

    #[test]
    fn map_status_error_categories() {
        assert!(matches!(
            map_status_error(429, String::new()),
            LlmError::RateLimited { .. }
        ));
        assert!(matches!(
            map_status_error(500, "err".into()),
            LlmError::Server { status: 500, .. }
        ));
        assert!(matches!(
            map_status_error(404, "not found".into()),
            LlmError::Client { status: 404, .. }
        ));
    }
}
