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

use futures::stream::{self, StreamExt};
use minicoding_core::model::{ContentBlock, LlmError, Message, Role, StopReason, ToolContent};
use minicoding_core::provider::{
    BoxFuture, BoxStream, Capabilities, ChatRequest, Delta, LlmProvider, Tokenizer, ToolCallDelta,
    Usage,
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
    /// 自定义显示名（`None` 时回退到 `PROVIDER_ID`）。
    display_name: Option<String>,
    api_base: String,
    model: String,
    client: reqwest::Client,
    tokenizer: Arc<TiktokenTokenizer>,
}

impl std::fmt::Debug for OllamaProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaProvider")
            .field("display_name", &self.display_name)
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
        Self::with_name(None, api_base, model)
    }

    /// 构造 provider 并指定自定义显示名。
    ///
    /// `display_name` 为 `None` 时 `id()` 回退到 `PROVIDER_ID`（`"ollama"`）。
    ///
    /// # Errors
    /// - `reqwest::Client` 初始化失败 → [`LlmError::Network`]
    /// - 分词器加载失败 → [`LlmError::Parse`]
    pub fn with_name(
        display_name: Option<String>,
        api_base: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        let model_str = model.into();
        let tokenizer = TiktokenTokenizer::new_for_model(&model_str).map_err(LlmError::Parse)?;
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;
        Ok(Self {
            display_name,
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
    fn id(&self) -> &str {
        self.display_name.as_deref().unwrap_or(PROVIDER_ID)
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
            // `Box::pin`（非 `.boxed()`）保留 `Send` 约束（见 openai.rs 同样注释）。
            let ndjson = crate::common::ndjson::from_response(resp);
            let delta_stream = ndjson.flat_map(|ev| {
                let items: Vec<Result<Delta, LlmError>> = match ev {
                    Ok(line) => match serde_json::from_str::<Value>(&line) {
                        Ok(json) => parse_chunk(&json).into_iter().map(Ok).collect(),
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

/// 将 [`Message`] 映射到 Ollama chat wire format（与 `OpenAI` 类似，但 `arguments` 为对象）。
fn message_to_ollama(m: &Message) -> Value {
    let role = role_str(&m.role);
    let text = extract_text(&m.content);

    // tool 响应消息：role=tool + tool_call_id + content
    // C-05：工具结果回灌 LLM 时包裹 `<tool_output>` 边界，防 LLM 把输出当指令执行。
    if m.role == Role::Tool {
        let mut obj = serde_json::Map::new();
        obj.insert("role".to_string(), Value::String(role.to_string()));
        // tool_call_id 优先取消息字段；运行时构造的 tool 消息只填在
        // `ContentBlock::ToolResult.call_id`（见 rt.rs tool_result_message），缺失时回退。
        if let Some(call_id) = crate::common::tool_call_id_of(m) {
            obj.insert("tool_call_id".to_string(), Value::String(call_id));
        }
        obj.insert(
            "content".to_string(),
            Value::String(crate::common::wrap_tool_output(&text)),
        );
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
        // 思考过程（deepseek-r1 等带 reasoning 的模型；部分接口用 reasoning_content）
        if let Some(reasoning) = message
            .get("reasoning")
            .or_else(|| message.get("reasoning_content"))
            .and_then(Value::as_str)
            && !reasoning.is_empty()
        {
            deltas.push(Delta::Reasoning(reasoning.to_string()));
        }
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
    #![allow(clippy::needless_pass_by_value)]

    use super::*;
    use futures::stream::StreamExt;
    use minicoding_core::model::{ToolContent, ToolSchema};
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
                model: "llama3".to_string(),
                temperature: None,
                top_p: None,
                max_output_tokens: None,
                stop: vec![],
                seed: None,
            },
        }
    }

    /// 把单个 JSON 值包装为 NDJSON 行（以 `\n` 结尾）。
    fn ndjson_line(json: &Value) -> String {
        format!("{json}\n")
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
        assert!(deltas.is_empty(), "expected empty: deltas");
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
        // C-05：工具结果回灌 LLM 时包裹 `<tool_output>` 边界
        assert_eq!(v["content"], "<tool_output>\nresult\n</tool_output>");
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

    // --- chat_stream HTTP mock 测试 ---

    #[tokio::test]
    async fn chat_stream_parses_text_delta() {
        // 场景：NDJSON 流含 message.content → Delta::Text；done=true → Delta::Stop
        let server = MockServer::start().await;
        let chunk = json!({
            "model": "llama3",
            "message": {"role": "assistant", "content": "hello"},
            "done": false
        });
        let done = json!({"model": "llama3", "done": true, "done_reason": "stop"});
        let body = format!("{}{}", ndjson_line(&chunk), ndjson_line(&done));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        // 1 text + 1 stop（无 token 统计）
        assert_eq!(deltas.len(), 2);
        assert!(matches!(&deltas[0], Delta::Text(t) if t == "hello"));
        assert!(matches!(&deltas[1], Delta::Stop(StopReason::EndTurn)));
    }

    #[tokio::test]
    async fn chat_stream_parses_tool_call() {
        // 场景：NDJSON 流含 tool_calls → Delta::ToolCall；done_reason="tools" → ToolUse
        let server = MockServer::start().await;
        let chunk = json!({
            "model": "llama3",
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "fs.read",
                        "arguments": {"path": "/tmp"},
                        "id": "call_01"
                    }
                }]
            },
            "done": false
        });
        let done = json!({"model": "llama3", "done": true, "done_reason": "tools"});
        let body = format!("{}{}", ndjson_line(&chunk), ndjson_line(&done));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        assert_eq!(deltas.len(), 2);
        match &deltas[0] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 0);
                assert_eq!(tc.name.as_deref(), Some("fs.read"));
                assert_eq!(tc.id.as_deref(), Some("call_01"));
                assert!(tc.args_chunk.as_ref().is_some_and(|s| s.contains("/tmp")));
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
        assert!(matches!(&deltas[1], Delta::Stop(StopReason::ToolUse)));
    }

    #[tokio::test]
    async fn chat_stream_done_with_usage() {
        // 场景：done=true 携带 token 统计 → Delta::Stop + Delta::Usage
        let server = MockServer::start().await;
        let done = json!({
            "model": "llama3",
            "done": true,
            "done_reason": "stop",
            "prompt_eval_count": 42,
            "eval_count": 10
        });
        let body = ndjson_line(&done);
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        assert_eq!(deltas.len(), 2);
        assert!(matches!(&deltas[0], Delta::Stop(StopReason::EndTurn)));
        match &deltas[1] {
            Delta::Usage(u) => {
                assert_eq!(u.input_tokens, 42);
                assert_eq!(u.output_tokens, 10);
            }
            other => panic!("期望 Usage，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_404_returns_client_error() {
        // 场景：HTTP 404 → LlmError::Client
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3").expect("构造 provider");
        let result = provider.chat_stream(basic_req()).await;
        let Err(err) = result else {
            panic!("404 应返回错误，但 chat_stream 成功");
        };
        match err {
            LlmError::Client { status, body } => {
                assert_eq!(status, 404);
                assert_eq!(body, "not found");
            }
            other => panic!("期望 Client 错误，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_429_returns_rate_limited() {
        // 场景：HTTP 429 限流 → LlmError::RateLimited（Ollama 不返回 Retry-After）
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3").expect("构造 provider");
        let result = provider.chat_stream(basic_req()).await;
        let Err(err) = result else {
            panic!("429 应返回错误，但 chat_stream 成功");
        };
        assert!(
            matches!(
                err,
                LlmError::RateLimited {
                    retry_after_ms: None
                }
            ),
            "期望 RateLimited(retry_after_ms=None)，得到 {err:?}"
        );
    }

    #[tokio::test]
    async fn chat_stream_500_returns_server_error() {
        // 场景：HTTP 500 服务端错误 → LlmError::Server
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3").expect("构造 provider");
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
    async fn chat_stream_connection_refused_returns_network_error() {
        // 场景：网络错误（连接被拒绝）→ LlmError::Network
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);

        let base = format!("http://{addr}");
        let provider = OllamaProvider::new(base, "llama3").expect("构造 provider");
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
    async fn chat_stream_invalid_json_returns_parse_error() {
        // 场景：NDJSON 行为非法 JSON → 流中返回 LlmError::Parse
        let server = MockServer::start().await;
        let body = "not valid json\n";
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3").expect("构造 provider");
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
    async fn chat_stream_sends_model_and_content_type() {
        // 场景：验证请求含 model 字段与 Content-Type 头（Ollama 无鉴权）
        let server = MockServer::start().await;
        let expected_body = json!({"model": "llama3"});
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(expected_body))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("{\"done\":true,\"done_reason\":\"stop\"}\n"),
            )
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "llama3").expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let _ = collect_deltas(stream).await;
    }

    // --- build_request_body 补充 ---

    #[test]
    fn build_request_body_with_tools_and_options() {
        let provider = OllamaProvider::new("http://localhost:11434", "llama3").expect("构造");
        let req = ChatRequest {
            system: "rules".to_string(),
            messages: vec![Message::user_text("hi")],
            tools: vec![ToolSchema {
                name: "fs.read".to_string(),
                description: "read a file".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            params: GenerationParams {
                model: "llama3".to_string(),
                temperature: Some(0.5),
                top_p: Some(0.9),
                max_output_tokens: Some(256),
                stop: vec!["END".to_string()],
                seed: Some(42),
            },
        };
        let body = provider.build_request_body(&req);
        assert_eq!(body["model"], "llama3");
        assert_eq!(body["stream"], true);
        // system 作为第一条 message（Ollama 接受 system role）
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "rules");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hi");
        // tools
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "fs.read");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
        // options（Ollama 风格：参数放 options 对象）
        assert_eq!(body["options"]["temperature"], json!(0.5_f32));
        assert_eq!(body["options"]["top_p"], json!(0.9_f32));
        assert_eq!(body["options"]["num_predict"], 256);
        assert_eq!(body["options"]["stop"], json!(["END"]));
        assert_eq!(body["options"]["seed"], 42);
    }

    #[test]
    fn build_request_body_no_system_no_tools_minimal() {
        let provider = OllamaProvider::new("http://localhost:11434", "llama3").expect("构造");
        let body = provider.build_request_body(&basic_req());
        assert_eq!(body["model"], "llama3");
        assert_eq!(body["stream"], true);
        // 无 system 时 messages 仅含用户消息
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
        // 无 tools 时不出现 tools 字段
        assert!(body.get("tools").is_none());
        // 无可选 params 时不出现 options 字段
        assert!(body.get("options").is_none());
    }

    // --- headers ---

    #[test]
    fn headers_only_content_type_no_auth() {
        // Ollama 无鉴权，headers 仅含 Content-Type
        let headers = OllamaProvider::headers();
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");
        // 不含 Authorization 头
        assert!(headers.get(reqwest::header::AUTHORIZATION).is_none());
    }

    // --- parse_chunk 补充 ---

    #[test]
    fn parse_chunk_multiple_tool_calls() {
        let chunk = json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"function": {"name": "fs.read", "arguments": {"path": "/a"}}},
                    {"function": {"name": "fs.write", "arguments": {"path": "/b"}}}
                ]
            },
            "done": false
        });
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 2);
        match &deltas[0] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 0);
                assert_eq!(tc.name.as_deref(), Some("fs.read"));
            }
            other => panic!("期望 ToolCall[0]，得到 {other:?}"),
        }
        match &deltas[1] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 1);
                assert_eq!(tc.name.as_deref(), Some("fs.write"));
            }
            other => panic!("期望 ToolCall[1]，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_chunk_done_reason_length_maps_max_tokens() {
        let chunk = json!({"done": true, "done_reason": "length"});
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Stop(StopReason::MaxTokens)));
    }

    #[test]
    fn parse_chunk_done_with_text_and_stop_in_same_line() {
        // 文本增量 + done=true 同时出现在一行
        let chunk = json!({
            "message": {"role": "assistant", "content": "final"},
            "done": true,
            "done_reason": "stop"
        });
        let deltas = parse_chunk(&chunk);
        // 1 text + 1 stop
        assert_eq!(deltas.len(), 2);
        assert!(matches!(&deltas[0], Delta::Text(t) if t == "final"));
        assert!(matches!(&deltas[1], Delta::Stop(StopReason::EndTurn)));
    }

    #[test]
    fn parse_chunk_reasoning_field_emits_reasoning() {
        // deepseek-r1 风格：message.reasoning 与 content 分离
        let chunk = json!({
            "message": {"role": "assistant", "reasoning": "先规划步骤", "content": "回答"}
        });
        let deltas = parse_chunk(&chunk);
        assert_eq!(deltas.len(), 2);
        assert!(matches!(&deltas[0], Delta::Reasoning(t) if t == "先规划步骤"));
        assert!(matches!(&deltas[1], Delta::Text(t) if t == "回答"));
    }

    #[test]
    fn parse_chunk_reasoning_content_fallback() {
        // 部分接口用 reasoning_content 字段
        let chunk = json!({
            "message": {"role": "assistant", "reasoning_content": "思路"}
        });
        let deltas = parse_chunk(&chunk);
        assert!(matches!(&deltas[0], Delta::Reasoning(t) if t == "思路"));
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

    // --- usize_from_json ---

    #[test]
    fn usize_from_json_boundaries() {
        assert_eq!(usize_from_json(Some(&json!(0))), 0);
        assert_eq!(usize_from_json(Some(&json!(42))), 42);
        assert_eq!(usize_from_json(None), 0);
        assert_eq!(usize_from_json(Some(&json!("not a number"))), 0);
        // 负数：as_u64 返回 None → 回 0
        assert_eq!(usize_from_json(Some(&json!(-1))), 0);
    }

    // --- provider 基本方法 ---

    #[test]
    fn provider_id_and_capabilities() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "llama3").expect("构造 provider");
        assert_eq!(provider.id(), PROVIDER_ID);
        let caps = provider.capabilities();
        assert!(caps.supports_tool_call);
        assert!(caps.supports_streaming);
        assert!(caps.supports_json_mode);
        assert!(!caps.supports_vision);
    }

    #[tokio::test]
    async fn count_tokens_delegates_to_tokenizer() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "llama3").expect("构造 provider");
        let n = provider
            .count_tokens(&[Message::user_text("hello world")])
            .await;
        assert!(n > 0, "count_tokens 应返回正数: {n}");
    }

    #[test]
    fn debug_format_does_not_include_sensitive_data() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "llama3").expect("构造 provider");
        let s = format!("{provider:?}");
        assert!(s.contains("OllamaProvider"), "Debug 应含结构体名: {s}");
        assert!(s.contains("llama3"), "Debug 应含 model: {s}");
        assert!(
            s.contains("http://localhost:11434"),
            "Debug 应含 api_base: {s}"
        );
    }
}
