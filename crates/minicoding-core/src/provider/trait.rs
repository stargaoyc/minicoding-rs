//! `LlmProvider` / `Tokenizer` trait + 流式 delta 类型。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容（Runtime 持有
//! `Arc<dyn LlmProvider>`）。等价于 `trait_variant::make` 生成的 Send 变体，
//! 但更显式且不依赖宏行为。

use crate::model::{LlmError, Message, ToolSchema};
use futures::Stream;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 异步返回类型（`Send` future，`dyn` 兼容）。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 流式返回类型（`Send` stream，`dyn` 兼容）。
///
/// 与 `futures::stream::BoxStream` 的区别：包含 `+ Send` 约束，使持有 stream
/// 跨 await 点的 future 仍是 `Send`（axum handler / `tokio::spawn` 需要）。
pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

/// LLM provider 能力声明。
#[derive(Debug, Clone)]
// Capabilities 是 provider 能力声明，bool 字段语义独立，不适合用 bitflags
#[allow(clippy::struct_excessive_bools)]
pub struct Capabilities {
    pub supports_tool_call: bool,
    pub supports_vision: bool,
    pub supports_streaming: bool,
    pub supports_json_mode: bool,
    pub context_window: usize,
    pub max_output: usize,
}

/// 生成参数。
#[derive(Debug, Clone)]
pub struct GenerationParams {
    pub model: String,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<usize>,
    pub stop: Vec<String>,
    pub seed: Option<u64>,
}

/// 一次对话请求。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: GenerationParams,
}

/// 流式增量。
#[derive(Debug, Clone)]
pub enum Delta {
    /// 文本增量。
    Text(String),
    /// 工具调用增量（分片聚合）。
    ToolCall(ToolCallDelta),
    /// token 用量统计。
    Usage(Usage),
    /// 停止。
    Stop(crate::model::StopReason),
}

/// 工具调用增量（OpenAI 风格分片）。
#[derive(Debug, Clone)]
pub struct ToolCallDelta {
    pub index: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    /// 增量 JSON 片段（需聚合后解析）。
    pub args_chunk: Option<String>,
}

/// token 用量统计。
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cache_read: Option<usize>,
    pub cache_write: Option<usize>,
}

/// LLM provider trait（可替换能力契约，`dyn` 兼容）。
///
/// 实现者写 `impl LlmProvider for MyProvider`，方法体用 `Box::pin(async move { ... })`。
/// Runtime 持有 `Arc<dyn LlmProvider>`。详见 `api.md` §3 dyn-compatibility 约定。
pub trait LlmProvider: Send + Sync {
    /// provider 标识（如 "openai"/"anthropic"），固定字符串。
    fn id(&self) -> &'static str;
    /// 能力声明。
    fn capabilities(&self) -> Capabilities;
    /// 关联的分词器。
    fn tokenizer(&self) -> Arc<dyn Tokenizer>;

    /// 流式对话。返回的 stream 必须 drop 即取消。
    fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<Delta, LlmError>>, LlmError>>;

    /// token 计数。
    fn count_tokens(&self, messages: &[Message]) -> BoxFuture<'_, usize>;
}

/// 分词器 trait（按 provider/model 选择实现，同步、`dyn` 兼容）。
pub trait Tokenizer: Send + Sync {
    /// 文本 token 数。
    fn count(&self, text: &str) -> usize;
    /// 消息序列 token 数。
    fn count_messages(&self, msgs: &[Message]) -> usize;
    /// 分词器标识（如 "cl100k"/"o200k"），固定字符串。
    fn id(&self) -> &'static str;
}

// `LlmError` → `RuntimeError` 由 `#[from]` 自动实现（见 `model::error`）。
