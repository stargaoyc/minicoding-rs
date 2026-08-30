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
    /// 自定义显示名（`None` 时回退到 `PROVIDER_ID`）。
    display_name: Option<String>,
    api_base: String,
    /// M-10：凭证重解析器（每次请求 resolve，缓存 ≤TTL；不再持有构造期一次性快照）。
    resolver: Arc<crate::common::CredentialResolver>,
    model: String,
    client: reqwest::Client,
    tokenizer: Arc<ApproxTokenizer>,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("display_name", &self.display_name)
            .field("api_base", &self.api_base)
            .field("model", &self.model)
            // 不输出凭证内容（C-04：日志脱敏）
            .field("api_key", &"<resolver>")
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
        Self::with_name(None, api_base, api_key, model)
    }

    /// 构造 provider 并指定自定义显示名。
    ///
    /// `display_name` 为 `None` 时 `id()` 回退到 `PROVIDER_ID`（`"anthropic"`）。
    ///
    /// # Errors
    /// `reqwest::Client` 初始化失败 → [`LlmError::Network`]
    pub fn with_name(
        display_name: Option<String>,
        api_base: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, LlmError> {
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
            model: model.into(),
            client,
            tokenizer: Arc::new(ApproxTokenizer),
        })
    }

    /// 构造 POST 请求体（Anthropic messages 格式，`stream: true`）。
    /// system prompt 放顶层 `system` 字段，不进 messages（见 `design.md` §4.2）。
    // 保留 `&self` 接收者（`unused_self`）：M-12 起 model 取自 `req.params.model`，
    // 改为关联函数会波及多处测试调用点。
    #[allow(clippy::unused_self)]
    fn build_request_body(&self, req: &ChatRequest) -> Value {
        let mut messages: Vec<Value> = req.messages.iter().map(message_to_anthropic).collect();
        // PTM-9（2026-08-26 R3 审查）：对话历史侧增量缓存断点——每 turn 增长的
        // messages 是长会话输入费用大头，仅在 system/tools 打断点时历史每轮
        // 全价重算。在最后一条消息的 content 尾块打第三个断点（上限 4 个内），
        // 下一轮该前缀命中缓存。注意：最后一条 assistant 消息（含 tool_use）
        // 的断点对"追加 tool_result"的增量模式同样有效。
        if let Some(last) = messages.last_mut() {
            let has_blocks = last
                .get("content")
                .is_some_and(|c| c.as_array().is_some_and(|a| !a.is_empty()));
            if has_blocks
                && let Some(arr) = last.get_mut("content").and_then(|c| c.as_array_mut())
                && let Some(last_block) = arr.last_mut()
            {
                // R8 PR-3 修复：最后一块是 `tool_result` 时**不放**断点——其内容
                // 每轮随工具输出变化，断点永不命中缓存（浪费一个断点位）。
                // 只对文本/工具调用等稳定前缀打断点。
                let is_tool_result = last_block
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t == "tool_result");
                if !is_tool_result {
                    last_block["cache_control"] = json!({ "type": "ephemeral" });
                }
            }
        }

        // PT4-1（R4）：空 model 回退到 provider 自身配置的默认模型
        let model = if req.params.model.is_empty() {
            &self.model
        } else {
            &req.params.model
        };
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            // max_tokens 计算见 compute_max_tokens（2026-08-25 审查 PR-4）
            "max_tokens": compute_max_tokens(
                req.params.max_output_tokens,
                req.params.thinking_budget_tokens,
            ),
        });

        // system prompt 顶层分离（Anthropic 不接受 messages 里的 system role）。
        // prompt caching（2026-08-23 审查遗留#2）：system 尾块打 `cache_control`
        // 断点——长会话 system（含 AGENTS.md/project doc）跨 turn 稳定，缓存命中
        // 可省 ~90% 输入费用；此前解析了 cache_read 字段却从不发送断点，永远
        // 打不进缓存。
        if !req.system.is_empty() {
            body["system"] = json!([
                { "type": "text", "text": req.system,
                  "cache_control": { "type": "ephemeral" } }
            ]);
        }

        if !req.tools.is_empty() {
            // 工具 schema 同样跨 turn 稳定：末位工具打第二个断点
            // （Anthropic 最多 4 个断点；system + tools 尾部两处覆盖最大稳定前缀）
            let mut tools: Vec<Value> = req
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
            if let Some(last) = tools.last_mut() {
                last["cache_control"] = json!({ "type": "ephemeral" });
            }
            body["tools"] = Value::Array(tools);
        }

        // Extended thinking（2026-08-23 审查遗留#2）：budget_tokens 显式启用；
        // Anthropic 要求 thinking 启用时不得携带 temperature——跳过之。
        if let Some(budget) = req.params.thinking_budget_tokens {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
        }

        if req.params.thinking_budget_tokens.is_none()
            && let Some(t) = req.params.temperature
        {
            body["temperature"] = json!(t);
        }
        // thinking 启用时 top_p 同样不可携带（Anthropic 与 temperature 同一约束：
        // thinking 模式下采样参数被拒绝），镜像 temperature 的 gate 逻辑
        // （2026-08-25 审查 PR-3）
        if req.params.thinking_budget_tokens.is_none()
            && let Some(t) = req.params.top_p
        {
            body["top_p"] = json!(t);
        }
        if !req.params.stop.is_empty() {
            body["stop_sequences"] = json!(req.params.stop);
        }
        body
    }

    /// 构造鉴权 headers（`x-api-key` + `anthropic-version`；M-10：每次请求重解析凭证）。
    fn auth_headers(&self) -> Result<HeaderMap, LlmError> {
        let key = self
            .resolver
            .resolve(PROVIDER_ID)?
            .ok_or(LlmError::NotConfigured)?;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&key)
                .map_err(|e| LlmError::Network(format!("invalid api key: {e}")))?,
        );
        Ok(headers)
    }
}

impl LlmProvider for AnthropicProvider {
    fn id(&self) -> &str {
        self.display_name.as_deref().unwrap_or(PROVIDER_ID)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tool_call: true,
            supports_vision: true,
            supports_streaming: true,
            supports_json_mode: false,
            // Claude 3.5 Sonnet 200K 上下文窗口
            context_window: 200_000,
            // PTM-14：与 MAX_OUTPUT_LIMIT 同步（8192 过时）。
            // PT-R7-1（2026-08-28 R7 审查）：与 `THINKING_MAX_OUTPUT_LIMIT`（64K）
            // 对齐——`compute_max_tokens` 的 thinking 路径实际产出可达 64K，此前
            // 声明 32K 使上游输出 token 预算预留不足（能力声明与实现上限不一致）。
            // 输出预算宁多勿少（声明的 max_output 只影响预留，不限制真实产出）。
            max_output: THINKING_MAX_OUTPUT_LIMIT,
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
            // PTM-3（2026-08-26 R3 审查）：extended thinking 与工具调用互斥 gate
            // ——Anthropic 要求最后一个 assistant turn 的 thinking 块（含
            // signature）随请求回传，而本通路不持久化 reasoning 块（trait 约定：
            // 仅流式展示），任何"思考→调工具→回灌结果"的第二跳必然 400。
            // 显式报错优于神秘 400；解除限制需 Message 持久化 thinking 块。
            if req.params.thinking_budget_tokens.is_some() && !req.tools.is_empty() {
                return Err(LlmError::Config(
                    "Anthropic extended thinking 与工具调用暂不支持组合：\
                     thinking 块（含 signature）未随会话持久化，第二跳请求必被 API 拒绝。\
                     请关闭 thinking_budget_tokens 或在无工具会话中使用"
                        .to_string(),
                ));
            }
            let body = self.build_request_body(&req);
            let url = format!("{}/v1/messages", self.api_base.trim_end_matches('/'));

            debug!(
                target: "minicoding::provider::anthropic",
                model = %self.model, url = %url, "POST v1/messages stream"
            );

            // Q4：发送/状态检查/行解码统一走 common::stream_runner
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

            // SSE data payload 按 `type` 分派（message_stop 等在 parse_event 内处理）
            let sse = crate::common::sse::from_response(resp);
            let delta_stream =
                crate::common::stream_runner::lines_to_deltas(Box::pin(sse), parse_event);

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
    // C-05：工具结果回灌 LLM 时包裹 `<tool_output>` 边界，防 LLM 把输出当指令执行。
    if m.role == Role::Tool {
        // tool_use_id 优先取消息字段；运行时构造的 tool 消息只填在
        // `ContentBlock::ToolResult.call_id`，缺失时回退（仍取不到则空串兜底）。
        let call_id = crate::common::tool_call_id_of(m).unwrap_or_default();
        let text = extract_text(&m.content);
        return json!({
            "role": role,
            "content": [{"type": "tool_result", "tool_use_id": call_id, "content": crate::common::wrap_tool_output(&text)}],
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
        // PTM-7（2026-08-26 R3 审查）：401/403 结构化为 AuthInvalid；
        // 400 + `prompt is too long` 识别为上下文超长
        401 | 403 => LlmError::AuthInvalid(body),
        s if (500..600).contains(&s) => LlmError::Server { status: s, body },
        s if s == 400 && body.contains("prompt is too long") => LlmError::ContextLength(body),
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
                    // 思考块增量（extended thinking，见 docs/design.md 提供商适配）
                    "thinking_delta" => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            deltas.push(Delta::Reasoning(text.to_string()));
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

/// Anthropic 能力声明的输出上限（PTM-14，2026-08-26 R3 审查：8192 → 32768
/// ——Claude 3.7+/4 系支持 32K-64K 输出，旧 clamp 会静默压低用户配置；
/// 32K 为跨模型安全上限，超出部分仍会被 API 以 400 拒绝并结构化报错）。
const MAX_OUTPUT_LIMIT: usize = 32_768;

/// thinking 路径的输出上限（PTM-2，2026-08-25 R2 审查）。
///
/// Claude thinking 模型（3.7+）实际支持最高 64K 输出 token——此前沿用
/// `MAX_OUTPUT_LIMIT`(8192) clamp，budget ≥ 8192 时产出 `max_tokens ≤
/// budget_tokens`，违反 API "输出预算必须大于思考预算"约束直接 400。
const THINKING_MAX_OUTPUT_LIMIT: usize = 64_000;

/// 计算 Anthropic `max_tokens` 请求值（2026-08-25 审查 PR-4 + R2 审查 PTM-2）。
///
/// - 未启用 thinking：用户配置值，缺省 4096（维持原行为），clamp 到 8192；
/// - 启用 thinking：输出预算必须**大于** `budget_tokens`（API 约束），取
///   `budget + max(用户配置或缺省值, 1024)`——此前仅 `budget + 1`，正文输出
///   余量趋近于零；上限用 [`THINKING_MAX_OUTPUT_LIMIT`]（64K），且保证结果
///   至少 `budget + 1`（clamp 后仍须满足严格大于，否则请求必 400）。
#[must_use]
fn compute_max_tokens(max_output_tokens: Option<usize>, thinking_budget: Option<u32>) -> usize {
    const DEFAULT_MAX_TOKENS: usize = 4_096;
    const THINKING_MIN_HEADROOM: usize = 1_024;
    match thinking_budget {
        // R4（PT4-10）：用户显式 `Some(0)` 时此前产出 `max_tokens: 0` 直接
        // 400——`max(1)` 保证最小合法值（0 通常表示"不限制"的误传）。
        None => max_output_tokens
            .unwrap_or(DEFAULT_MAX_TOKENS)
            .clamp(1, MAX_OUTPUT_LIMIT),
        Some(budget) => {
            let headroom = max_output_tokens
                .unwrap_or(DEFAULT_MAX_TOKENS)
                .max(THINKING_MIN_HEADROOM);
            // u32 在 64 位平台必能装入 usize；转换失败（16 位平台）按 0 兜底
            let raw = usize::try_from(budget)
                .unwrap_or_default()
                .saturating_add(headroom);
            // clamp 到 thinking 上限后仍必须严格大于 budget（API 约束）
            let clamped = raw.min(THINKING_MAX_OUTPUT_LIMIT);
            let budget_usize = usize::try_from(budget).unwrap_or_default();
            clamped.max(budget_usize.saturating_add(1))
        }
    }
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

/// 近似分词器（Anthropic 未公开分词器，`design.md` §4.4）。
///
/// 估算策略（2026-08-23 审查 §5-P2 修正）：CJK 字符按 **1 token/字** 计权
/// （Claude 分词器对汉字实测约 1~1.5 token/字），其余字符按 4 字符 ≈ 1 token。
/// 此前统一 `chars / 4` 对中文低估约 4 倍，导致压缩预算判定显著滞后、长中文
/// 会话在真实超限后才触发压缩。
#[derive(Debug, Default)]
pub struct ApproxTokenizer;

impl ApproxTokenizer {
    /// 单文本估算：CJK 字符逐字计 1 token，其余字符 4 字符 ≈ 1 token。
    ///
    /// R9 PROV-1 修复（系统性低估两处）：
    /// - `\uXXXX` 转义序列（工具参数/结果常以 JSON 序列化进入上下文）：按
    ///   解码后的字符计数——此前按 6 个 ASCII 字符（`6/4≈2` token）计，
    ///   JSON 转义中文实测低估 −52.6%；
    /// - 非 BMP 字符（emoji/补充平面）：保守计 4 token/个——此前按 1 个
    ///   普通字符（`1/4→0` token）计，5 个 emoji 实测真实 13 token 被估成 2
    ///   （−84.6%）。4 token/个为保守上界，低估方向收口。
    fn count_str(text: &str) -> usize {
        let mut cjk = 0usize;
        let mut emoji = 0usize;
        let mut other = 0usize;
        let mut rest = text;
        while !rest.is_empty() {
            // `\uXXXX` 转义序列：解码后按实际字符类别计数
            if let Some(after) = rest.strip_prefix("\\u") {
                let hex: String = after.chars().take(4).collect();
                if hex.len() == 4
                    && hex.chars().all(|c| c.is_ascii_hexdigit())
                    && let Ok(code) = u32::from_str_radix(&hex, 16)
                {
                    let decoded = char::from_u32(code).unwrap_or('\u{FFFD}');
                    if is_cjk_char(decoded) {
                        cjk += 1;
                    } else if code >= 0x10000 {
                        emoji += 1;
                    } else {
                        other += 1;
                    }
                    rest = &after[4..];
                    continue;
                }
            }
            let c = rest.chars().next().unwrap_or_default();
            if is_cjk_char(c) {
                cjk += 1;
            } else if u32::from(c) >= 0x10000 {
                // 非 BMP（emoji/补充平面）：保守 4 token/个
                emoji += 1;
            } else {
                other += 1;
            }
            rest = &rest[c.len_utf8()..];
        }
        cjk + emoji * 4 + other.div_ceil(4)
    }
}

/// 判断字符是否为 CJK（中日韩表意文字/假名/谚文/CJK 标点/全角形式）。
///
/// 覆盖常用区间即可——token 估算用途，不追求 Unicode 全集；
/// std 无 `char::is_cjk`，手写区间避免引入 `unicode-script` 类重依赖。
fn is_cjk_char(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F     // CJK 符号与标点
        | 0x3040..=0x30FF   // 平假名/片假名
        | 0x3400..=0x4DBF   // 表意文字扩展 A
        | 0x4E00..=0x9FFF   // CJK 统一表意文字基本区
        | 0xAC00..=0xD7AF   // 谚文音节
        | 0xF900..=0xFAFF   // CJK 兼容表意文字
        | 0xFF00..=0xFFEF   // 全角形式
        | 0x20000..=0x2FA1F // 表意文字扩展 B–F
    )
}

impl Tokenizer for ApproxTokenizer {
    fn count(&self, text: &str) -> usize {
        Self::count_str(text)
    }
    fn count_messages(&self, msgs: &[Message]) -> usize {
        msgs.iter()
            .map(|m| {
                // 每条消息加 4 token overhead（角色标记等），与 tiktoken 习惯对齐
                // PTM-2（2026-08-26 R3 审查）：用 `full_text()`（含 tool_calls 的
                // name+args JSON）而非 `extract_text`——agentic 会话中工具调用
                // JSON 往往是 token 大头，漏计导致压缩触发滞后、真实超窗 400
                // （tiktoken 侧 §8-P0 同型修复未同步到此处）。
                4 + Self::count_str(&m.full_text())
            })
            .sum()
    }
    fn id(&self) -> &'static str {
        "anthropic-approx"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::needless_pass_by_value)]

    use super::*;
    use futures::stream::StreamExt;
    use minicoding_core::model::ToolSchema;
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
                model: "claude-3-5-sonnet".to_string(),
                temperature: None,
                top_p: None,
                max_output_tokens: None,
                stop: vec![],
                seed: None,
                thinking_budget_tokens: None,
                cache_key: None,
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
        // C-05：工具结果回灌 LLM 时包裹 `<tool_output>` 边界
        assert_eq!(
            v["content"][0]["content"],
            "<tool_output>\nresult\n</tool_output>"
        );
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
    fn build_request_body_prompt_caching_breakpoints() {
        // 2026-08-23 审查遗留#2：system 尾块与末位工具打 cache_control 断点
        let provider =
            AnthropicProvider::new("https://api.anthropic.com", "sk-test", "claude-3-5-sonnet")
                .expect("构造");
        let req = ChatRequest {
            system: "rules".into(),
            messages: vec![],
            tools: vec![
                ToolSchema {
                    name: "a".into(),
                    description: String::new(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
                ToolSchema {
                    name: "b".into(),
                    description: String::new(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
            ],
            params: minicoding_core::provider::GenerationParams {
                model: "claude-3-5-sonnet".into(),
                temperature: None,
                top_p: None,
                max_output_tokens: Some(1_024),
                stop: vec![],
                seed: None,
                thinking_budget_tokens: None,
                cache_key: None,
            },
        };
        let body = provider.build_request_body(&req);
        assert_eq!(
            body["system"][0]["cache_control"]["type"],
            json!("ephemeral"),
            "system 尾块应打缓存断点"
        );
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            body["tools"][1]["cache_control"]["type"],
            json!("ephemeral")
        );
    }

    #[test]
    fn build_request_body_thinking_disables_temperature() {
        let provider =
            AnthropicProvider::new("https://api.anthropic.com", "sk-test", "claude-3-5-sonnet")
                .expect("构造");
        let req = ChatRequest {
            system: String::new(),
            messages: vec![],
            tools: vec![],
            params: minicoding_core::provider::GenerationParams {
                model: "claude-3-5-sonnet".into(),
                temperature: Some(0.7),
                top_p: None,
                max_output_tokens: Some(2_000),
                stop: vec![],
                seed: None,
                thinking_budget_tokens: Some(1_500),
                cache_key: None,
            },
        };
        let body = provider.build_request_body(&req);
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["thinking"]["budget_tokens"], 1_500);
        assert!(
            body.get("temperature").is_none(),
            "thinking 启用时应省略 temperature"
        );
        // PR-4：thinking 时 max_tokens = budget + max(用户配置, 1024) = 1500 + 2000
        assert_eq!(body["max_tokens"], 3_500);
    }

    #[test]
    fn build_request_body_thinking_gates_top_p() {
        // PR-3（2026-08-25 审查）：thinking 启用时 top_p 必须省略（镜像 temperature）
        let provider =
            AnthropicProvider::new("https://api.anthropic.com", "sk-test", "claude-3-5-sonnet")
                .expect("构造");
        let req = ChatRequest {
            system: String::new(),
            messages: vec![],
            tools: vec![],
            params: minicoding_core::provider::GenerationParams {
                model: "claude-3-5-sonnet".into(),
                temperature: None,
                top_p: Some(0.9),
                max_output_tokens: Some(2_000),
                stop: vec![],
                seed: None,
                thinking_budget_tokens: Some(1_000),
                cache_key: None,
            },
        };
        let body = provider.build_request_body(&req);
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert!(body.get("top_p").is_none(), "thinking 启用时应省略 top_p");
        // 非 thinking 路径不受影响：同请求去掉 budget 后 top_p 应保留
        let mut req2 = req.clone();
        req2.params.thinking_budget_tokens = None;
        let body2 = provider.build_request_body(&req2);
        assert_eq!(body2["top_p"], json!(0.9_f32));
    }

    #[test]
    fn compute_max_tokens_matrix() {
        // 未启用 thinking：用户值 / 缺省 4096，不 clamp 旧路径行为
        assert_eq!(compute_max_tokens(None, None), 4_096);
        assert_eq!(compute_max_tokens(Some(512), None), 512);
        assert_eq!(compute_max_tokens(Some(9_999), None), 9_999);
        // PTM-14：clamp 上限提升至 32K（Claude 3.7+/4 支持），超限仍被钳制
        assert_eq!(compute_max_tokens(Some(99_999), None), MAX_OUTPUT_LIMIT);
        // thinking：budget + max(用户配置, 1024)
        assert_eq!(compute_max_tokens(None, Some(1_000)), 5_096);
        // 小配置走最小余量 1024
        assert_eq!(compute_max_tokens(Some(100), Some(500)), 1_524);
        // PTM-2：clamp 到 thinking 上限后仍必须严格大于 budget（API 约束）——
        // 此前 clamp 到 8192 使 budget≥8192 时 max_tokens ≤ budget 直接 400
        assert_eq!(compute_max_tokens(Some(8_192), Some(8_000)), 8_000 + 8_192);
        assert_eq!(
            compute_max_tokens(None, Some(9_000)),
            9_000 + 4_096,
            "budget+缺省余量，低于 64K 上限不 clamp"
        );
        // budget 本身超过上限时，"严格大于 budget"的 API 约束优先于 clamp——
        // 违反前者必 400；后者由上游对 budget 的合法性校验兜底
        let over_budget: u32 =
            u32::try_from(THINKING_MAX_OUTPUT_LIMIT + 1_000).expect("fits in u32 on 64-bit");
        assert_eq!(
            compute_max_tokens(None, Some(over_budget)),
            THINKING_MAX_OUTPUT_LIMIT + 1_001
        );
        assert_eq!(
            compute_max_tokens(None, Some(63_000)),
            THINKING_MAX_OUTPUT_LIMIT,
            "budget < 上限：clamp 后仍严格大于 budget"
        );
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
                thinking_budget_tokens: None,
                cache_key: None,
            },
        };
        let body = provider.build_request_body(&req);
        // system 顶层分离，不在 messages 里
        // 缓存断点后 system 为单元素数组（text + cache_control）
        assert_eq!(body["system"][0]["text"], "You are helpful.");
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

    // --- chat_stream HTTP mock 测试 ---

    #[tokio::test]
    async fn chat_stream_parses_text_delta() {
        // 场景：mock 返回 SSE 流含 content_block_delta(text_delta) → Delta::Text
        let server = MockServer::start().await;
        let event = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hello"}
        });
        let sse_body = sse_event(&event);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet")
            .expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Text(t) if t == "hello"));
    }

    #[tokio::test]
    async fn chat_stream_parses_tool_use_and_input_json_delta() {
        // 场景：content_block_start(tool_use) + content_block_delta(input_json_delta)
        let server = MockServer::start().await;
        let start = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_01", "name": "fs.read", "input": {}}
        });
        let delta = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"/tmp\"}"}
        });
        let sse_body = format!("{}{}", sse_event(&start), sse_event(&delta));
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet")
            .expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        assert_eq!(deltas.len(), 2);
        match &deltas[0] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.index, 0);
                assert_eq!(tc.id.as_deref(), Some("toolu_01"));
                assert_eq!(tc.name.as_deref(), Some("fs.read"));
            }
            other => panic!("期望 ToolCall，得到 {other:?}"),
        }
        match &deltas[1] {
            Delta::ToolCall(tc) => {
                assert_eq!(tc.args_chunk.as_deref(), Some("{\"path\":\"/tmp\"}"));
            }
            other => panic!("期望 ToolCall(args)，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_emits_stop_and_usage() {
        // 场景：message_delta 含 stop_reason + output_tokens → Delta::Stop + Delta::Usage
        let server = MockServer::start().await;
        let text_chunk = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "ok"}
        });
        let stop_chunk = json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 5}
        });
        let sse_body = format!("{}{}", sse_event(&text_chunk), sse_event(&stop_chunk));
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet")
            .expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        assert_eq!(deltas.len(), 3);
        assert!(matches!(&deltas[0], Delta::Text(t) if t == "ok"));
        assert!(matches!(&deltas[1], Delta::Stop(StopReason::EndTurn)));
        assert!(matches!(&deltas[2], Delta::Usage(_)));
    }

    #[tokio::test]
    async fn chat_stream_message_stop_terminates_cleanly() {
        // 场景：message_stop 事件不产出 delta，流正常终止
        let server = MockServer::start().await;
        let sse_body = sse_event(&json!({"type": "message_stop"}));
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet")
            .expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let deltas = collect_deltas(stream).await;
        assert!(deltas.is_empty(), "expected empty: deltas");
    }

    #[tokio::test]
    async fn chat_stream_401_returns_client_error() {
        // 场景：HTTP 401 鉴权失败 → LlmError::AuthInvalid（PTM-7 结构化分类）
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet")
            .expect("构造 provider");
        let result = provider.chat_stream(basic_req()).await;
        let Err(err) = result else {
            panic!("401 应返回错误，但 chat_stream 成功");
        };
        match err {
            LlmError::AuthInvalid(body) => {
                assert_eq!(body, "unauthorized");
            }
            other => panic!("期望 AuthInvalid 错误，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_429_returns_rate_limited_with_retry_after() {
        // 场景：HTTP 429 限流 + Retry-After → LlmError::RateLimited（携带毫秒）
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_string("slow down")
                    .insert_header("retry-after", "3"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet")
            .expect("构造 provider");
        let result = provider.chat_stream(basic_req()).await;
        let Err(err) = result else {
            panic!("429 应返回错误，但 chat_stream 成功");
        };
        match err {
            LlmError::RateLimited { retry_after_ms } => {
                assert_eq!(retry_after_ms, Some(3000));
            }
            other => panic!("期望 RateLimited 错误，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_500_returns_server_error() {
        // 场景：HTTP 500 服务端错误 → LlmError::Server
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet")
            .expect("构造 provider");
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
        let provider =
            AnthropicProvider::new(base, "sk-test", "claude-3-5-sonnet").expect("构造 provider");
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
        // 场景：SSE data 为非法 JSON → 流中返回 LlmError::Parse
        let server = MockServer::start().await;
        let sse_body = "data: not valid json\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet")
            .expect("构造 provider");
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
    async fn chat_stream_sends_x_api_key_and_anthropic_version() {
        // 场景：验证请求含 x-api-key、anthropic-version 头与 model 字段
        let server = MockServer::start().await;
        let expected_body = json!({"model": "claude-3-5-sonnet"});
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-test"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(body_partial_json(expected_body))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("data: {\"type\":\"message_stop\"}\n\n")
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(server.uri(), "sk-test", "claude-3-5-sonnet")
            .expect("构造 provider");
        let stream = provider
            .chat_stream(basic_req())
            .await
            .expect("chat_stream");
        let _ = collect_deltas(stream).await;
    }

    // --- build_request_body 补充 ---

    #[test]
    fn build_request_body_with_tools_and_params() {
        let provider =
            AnthropicProvider::new("https://api.anthropic.com", "sk-test", "claude-3-5-sonnet")
                .expect("构造");
        let req = ChatRequest {
            system: "rules".to_string(),
            messages: vec![Message::user_text("hi")],
            tools: vec![ToolSchema {
                name: "fs.read".to_string(),
                description: "read a file".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            params: GenerationParams {
                model: "claude-3-5-sonnet".to_string(),
                temperature: Some(0.7),
                top_p: Some(0.9),
                max_output_tokens: Some(512),
                stop: vec!["END".to_string()],
                seed: None,
                thinking_budget_tokens: None,
                cache_key: None,
            },
        };
        let body = provider.build_request_body(&req);
        assert_eq!(body["model"], "claude-3-5-sonnet");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["system"][0]["text"], "rules");
        assert_eq!(body["tools"][0]["name"], "fs.read");
        assert_eq!(body["tools"][0]["description"], "read a file");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["temperature"], json!(0.7_f32));
        assert_eq!(body["top_p"], json!(0.9_f32));
        assert_eq!(body["stop_sequences"], json!(["END"]));
        // Anthropic 不支持 seed 参数
        assert!(body.get("seed").is_none());
    }

    #[test]
    fn build_request_body_default_max_tokens_when_absent() {
        let provider =
            AnthropicProvider::new("https://api.anthropic.com", "sk-test", "claude-3-5-sonnet")
                .expect("构造");
        let body = provider.build_request_body(&basic_req());
        // max_output_tokens 缺省 4096
        assert_eq!(body["max_tokens"], 4_096);
    }

    #[test]
    fn build_request_body_no_system_no_tools_minimal() {
        let provider =
            AnthropicProvider::new("https://api.anthropic.com", "sk-test", "claude-3-5-sonnet")
                .expect("构造");
        let body = provider.build_request_body(&basic_req());
        assert_eq!(body["model"], "claude-3-5-sonnet");
        assert_eq!(body["stream"], true);
        // 无 system 时不出现 system 字段
        assert!(body.get("system").is_none());
        // 无 tools 时不出现 tools 字段
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
    }

    // --- auth_headers ---

    #[test]
    fn auth_headers_includes_x_api_key_and_version() {
        let provider = AnthropicProvider::new(
            "https://api.anthropic.com",
            "sk-test-key",
            "claude-3-5-sonnet",
        )
        .expect("构造");
        let headers = provider.auth_headers().expect("构造 headers");
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(headers.get("x-api-key").unwrap(), "sk-test-key");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn auth_headers_invalid_api_key_returns_network_error() {
        // 包含换行符的 api_key 无法构造 HeaderValue → LlmError::Network
        let provider = AnthropicProvider::new(
            "https://api.anthropic.com",
            "sk-bad\nkey",
            "claude-3-5-sonnet",
        )
        .expect("构造");
        let result = provider.auth_headers();
        let Err(err) = result else {
            panic!("非法 api_key 应返回错误");
        };
        assert!(
            matches!(err, LlmError::Network(_)),
            "期望 Network 错误，得到 {err:?}"
        );
    }

    // --- parse_usage ---

    #[test]
    fn parse_usage_with_cache_fields() {
        let u = parse_usage(&json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 30,
            "cache_creation_input_tokens": 10
        }));
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.cache_read, Some(30));
        assert_eq!(u.cache_write, Some(10));
    }

    #[test]
    fn parse_usage_missing_fields_default_zero() {
        let u = parse_usage(&json!({}));
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
        assert!(u.cache_read.is_none());
        assert!(u.cache_write.is_none());
    }

    // --- parse_event 边界 ---

    #[test]
    fn parse_content_block_start_non_tool_use_skipped() {
        // content_block 类型为 text（非 tool_use）→ 不产出 delta
        let ev = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        });
        assert!(parse_event(&ev).is_empty());
    }

    #[test]
    fn parse_text_delta_missing_text_skipped() {
        let ev = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta"}
        });
        assert!(parse_event(&ev).is_empty());
    }

    #[test]
    fn parse_thinking_delta_emits_reasoning() {
        // extended thinking：thinking_delta 增量（与 text_delta 并行、独立字段）
        let ev = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "先分析需求"}
        });
        let deltas = parse_event(&ev);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Reasoning(t) if t == "先分析需求"));
    }

    #[test]
    fn parse_thinking_delta_missing_thinking_skipped() {
        let ev = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "thinking_delta"}
        });
        assert!(parse_event(&ev).is_empty());
    }

    #[test]
    fn parse_message_delta_empty_stop_reason_skipped() {
        // stop_reason 为空字符串 → 不产出 Stop delta（仅可能有 Usage）
        let ev = json!({
            "type": "message_delta",
            "delta": {"stop_reason": ""},
            "usage": {"output_tokens": 3}
        });
        let deltas = parse_event(&ev);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], Delta::Usage(_)));
    }

    #[test]
    fn parse_unknown_event_type_skipped() {
        let ev = json!({"type": "some_unknown_type", "data": "irrelevant"});
        assert!(parse_event(&ev).is_empty());
    }

    #[test]
    fn parse_event_missing_type_skipped() {
        // 无 type 字段 → unwrap_or("") → 走默认分支
        let ev = json!({"data": "irrelevant"});
        assert!(parse_event(&ev).is_empty());
    }

    // --- content_to_blocks ---

    #[test]
    fn content_to_blocks_empty_returns_default_text() {
        let blocks = content_to_blocks(&[]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "");
    }

    #[test]
    fn content_to_blocks_text_and_image() {
        let blocks = content_to_blocks(&[
            ContentBlock::Text {
                text: "hello".into(),
            },
            ContentBlock::Image {
                mime: "image/png".into(),
                data: "base64data".into(),
            },
        ]);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "base64data");
    }

    #[test]
    fn content_to_blocks_ignores_tool_use_and_fills_default() {
        let blocks =
            content_to_blocks(&[ContentBlock::ToolUse(minicoding_core::model::ToolCall {
                id: "call_1".into(),
                name: "noop".into(),
                input: json!({}),
            })]);
        // ToolUse 被忽略，但空 blocks 会填充默认 text
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
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

    // --- map_status_error ---

    #[test]
    fn map_status_error_categories() {
        assert!(matches!(
            map_status_error(401, "unauth".into(), None),
            LlmError::AuthInvalid(_)
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

    // --- ApproxTokenizer ---

    #[test]
    fn approx_tokenizer_count_messages_includes_overhead() {
        let tok = ApproxTokenizer;
        let n = tok.count_messages(&[Message::user_text("abcdefgh")]);
        // 8 字符 / 4 = 2 token + 4 overhead = 6
        assert_eq!(n, 6);
    }

    #[test]
    fn approx_tokenizer_cjk_weighting() {
        let tok = ApproxTokenizer;
        // CJK 每字 1 token：4 个汉字 = 4 token（此前 chars/4 仅算 1，低估 4 倍）
        assert_eq!(tok.count("你好世界"), 4);
        // 混合文本：2 CJK + 8 ASCII → 2 + ceil(8/4) = 4
        assert_eq!(tok.count("你好abcdefgh"), 4);
        // count_messages 同口径
        let n = tok.count_messages(&[Message::user_text("你好世界")]);
        assert_eq!(n, 4 + 4);
    }

    #[test]
    fn approx_tokenizer_json_escaped_unicode_weighted_by_decoded_char() {
        // R9 PROV-1：`\u4f60\u597d`（"你好"）此前按 6 ASCII 字符计
        // ceil(12/4)=3 token，JSON 转义中文实测低估 −52.6%。解码后按 CJK
        // 逐字 1 token = 2 token（低估方向收口，仍偏保守）。
        let tok = ApproxTokenizer;
        assert_eq!(tok.count(r"\u4f60\u597d"), 2, "转义 CJK 应按解码后逐字计");
        // 混合：转义中文 + ASCII
        assert_eq!(tok.count(r"\u4f60\u597d abcdef"), 2 + 2);
        // 转义 ASCII（\u0041 = 'A'）按普通字符计
        assert_eq!(tok.count(r"\u0041"), 1);
    }

    #[test]
    fn approx_tokenizer_non_bmp_emoji_conservative_weighting() {
        // R9 PROV-1：emoji 此前按 1 普通字符计（1/4→0 token），5 个 emoji
        // 真实 13 token 被估成 2（−84.6%）。修复后非 BMP 保守 4 token/个。
        let tok = ApproxTokenizer;
        // 5 个 emoji（U+1F600）→ 5 * 4 = 20 token（低估方向收口）
        assert_eq!(tok.count("😀😀😀😀😀"), 20);
        // 混合：4 emoji + 空格 + 8 ASCII → 4*4 + ceil(9/4)=3 → 19
        assert_eq!(tok.count("😀😀😀😀 abcdefgh"), 19);
    }

    // --- provider 基本方法 ---

    #[test]
    fn provider_id_and_capabilities() {
        let provider =
            AnthropicProvider::new("https://api.anthropic.com", "sk-test", "claude-3-5-sonnet")
                .expect("构造 provider");
        assert_eq!(provider.id(), PROVIDER_ID);
        let caps = provider.capabilities();
        assert!(caps.supports_tool_call);
        assert!(caps.supports_streaming);
        assert!(caps.supports_vision);
        assert!(!caps.supports_json_mode);
    }

    #[tokio::test]
    async fn count_tokens_delegates_to_tokenizer() {
        let provider =
            AnthropicProvider::new("https://api.anthropic.com", "sk-test", "claude-3-5-sonnet")
                .expect("构造 provider");
        let n = provider
            .count_tokens(&[Message::user_text("hello world")])
            .await;
        assert!(n > 0, "count_tokens 应返回正数: {n}");
    }

    #[test]
    fn debug_does_not_leak_api_key() {
        // C-04：Debug 输出脱敏 api_key（前 4 字符 + ***）
        let provider = AnthropicProvider::new(
            "https://api.anthropic.com",
            "sk-secret-12345",
            "claude-3-5-sonnet",
        )
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
}
