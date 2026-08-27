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
    /// Extended thinking 预算（token，2026-08-23 审查遗留#2）。
    ///
    /// `Some(n)` 且 provider 支持时启用思考模式（Anthropic `thinking.budget_tokens`，
    /// `OpenAI` 映射为 `reasoning_effort` 由各实现自行决策）；`None` 保持默认。
    /// 注意：budget 应显著小于 `max_output_tokens`（Anthropic 要求 thinking
    /// 计入输出预算）。
    pub thinking_budget_tokens: Option<u32>,
    /// PTM-9（2026-08-26 R3 审查）：会话级稳定缓存路由键（OpenAI
    /// `prompt_cache_key`）。`Some` 时 provider 下发以提升 prompt cache 命中；
    /// `None` 不发送（Ollama 等无此概念的 provider 忽略）。
    #[doc(hidden)]
    pub cache_key: Option<String>,
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
    /// 思考过程增量（reasoning/thinking，与正文分开下发；不进消息正文，仅流式展示）。
    Reasoning(String),
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

impl Usage {
    /// PTM-1（2026-08-26 R3 审查）：流式 Usage 合并语义。
    ///
    /// Anthropic `message_start` 携带完整 input/cache 计量，而后续
    /// `message_delta` 只带 `output_tokens`——若整包替换会把输入侧计量覆盖
    /// 为 0。合并规则：`output_tokens` 取较大值（增量累计型）；`input_tokens`
    /// 与 cache 字段取"新值非零/非 None 则替换，否则保留旧值"（快照型）。
    pub fn merge_incremental(&mut self, newer: &Usage) {
        self.output_tokens = self.output_tokens.max(newer.output_tokens);
        if newer.input_tokens > 0 {
            self.input_tokens = newer.input_tokens;
        }
        if newer.cache_read.is_some() {
            self.cache_read = newer.cache_read;
        }
        if newer.cache_write.is_some() {
            self.cache_write = newer.cache_write;
        }
    }
}

/// LLM provider trait（可替换能力契约，`dyn` 兼容）。
///
/// 实现者写 `impl LlmProvider for MyProvider`，方法体用 `Box::pin(async move { ... })`。
/// Runtime 持有 `Arc<dyn LlmProvider>`。详见 `api.md` §3 dyn-compatibility 约定。
pub trait LlmProvider: Send + Sync {
    /// provider 显示名（如 `"openai"`/`"anthropic"`/`"deepseek"`）。
    ///
    /// 返回 `&str` 而非 `&'static str`，允许实现者存储用户配置的自定义名称
    /// （`ProviderConfig::name`）。未配置自定义名称时回退到 provider 类型常量
    /// （如 `PROVIDER_ID = "openai"`）。
    fn id(&self) -> &str;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_incremental_preserves_input_tokens() {
        // PTM-1 message_start{input:100,cache_read:Some(50),output:1}
        // → message_delta{output:20}：input 与 cache 保留旧值
        let mut u = Usage {
            input_tokens: 100,
            output_tokens: 1,
            cache_read: Some(50),
            cache_write: None,
        };
        u.merge_incremental(&Usage {
            input_tokens: 0,
            output_tokens: 20,
            cache_read: None,
            cache_write: None,
        });
        assert_eq!(u.input_tokens, 100, "input 保留旧值");
        assert_eq!(u.output_tokens, 20, "output 取较大值");
        assert_eq!(u.cache_read, Some(50), "cache_read 保留旧值");
    }

    #[test]
    fn merge_incremental_replaces_nonzero_input() {
        // 后续 chunk 带新 input 值时替换（OpenAI 兼容网关每 chunk 发快照）
        let mut u = Usage {
            input_tokens: 100,
            output_tokens: 5,
            cache_read: None,
            cache_write: None,
        };
        u.merge_incremental(&Usage {
            input_tokens: 120,
            output_tokens: 10,
            cache_read: None,
            cache_write: None,
        });
        assert_eq!(u.input_tokens, 120, "input 新值非零时替换");
        assert_eq!(u.output_tokens, 10, "output 取较大值");
    }

    #[test]
    fn merge_incremental_some_none_does_not_overwrite_cache() {
        let mut u = Usage {
            input_tokens: 50,
            output_tokens: 3,
            cache_read: Some(30),
            cache_write: Some(10),
        };
        // newer 全部 None —— 保留旧值
        u.merge_incremental(&Usage {
            input_tokens: 0,
            output_tokens: 5,
            cache_read: None,
            cache_write: None,
        });
        assert_eq!(u.cache_read, Some(30));
        assert_eq!(u.cache_write, Some(10));
    }

    #[test]
    fn merge_incremental_none_does_not_replace_with_zero() {
        // 0 值 output_tokens 不会降低计数（max 语义）
        let mut u = Usage {
            input_tokens: 50,
            output_tokens: 100,
            cache_read: None,
            cache_write: None,
        };
        u.merge_incremental(&Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read: None,
            cache_write: None,
        });
        assert_eq!(u.output_tokens, 100, "0 不降低 output");
    }
}
