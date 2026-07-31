//! `tiktoken-rs` 分词器实现。
//!
//! 包装 `tiktoken_rs::CoreBPE`，按 model 选择 `cl100k_base`（gpt-4/gpt-3.5 等）
//! 或 `o200k_base`（gpt-4o 系列）。`Tokenizer` trait 同步、`dyn` 兼容，由
//! `LlmProvider::tokenizer` 返回 `Arc<dyn Tokenizer>` 供 Runtime 与上下文管理器调用。

use minicoding_core::model::{Message, Role};
use minicoding_core::provider::Tokenizer;
use tiktoken_rs::CoreBPE;

/// `tiktoken` 编码器种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiktokenKind {
    /// `cl100k_base`，用于 `gpt-4` / `gpt-4-turbo` / `gpt-3.5-turbo` 等。
    Cl100k,
    /// `o200k_base`，用于 `gpt-4o` / `gpt-4o-mini` / `o1` / `o3` 系列。
    O200k,
}

impl TiktokenKind {
    /// 返回分词器标识字符串（与 `Tokenizer::id` 一致）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cl100k => "cl100k",
            Self::O200k => "o200k",
        }
    }
}

/// 基于 `tiktoken-rs` 的分词器实现。
pub struct TiktokenTokenizer {
    bpe: CoreBPE,
    kind: TiktokenKind,
}

// bpe 词表大且为内部细节，无需在 Debug 输出中展示
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for TiktokenTokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TiktokenTokenizer")
            .field("kind", &self.kind)
            .finish()
    }
}

/// 单条 chat 消息的固定开销（OpenAI 推荐的 `<|im_start|>{role}\n{content}<|im_end|>\n` 估算）。
const TOKENS_PER_MESSAGE: usize = 3;
/// 每条带 name 的消息额外开销（这里 role 视作 name，统一 +1）。
const TOKENS_PER_NAME: usize = 1;
/// 回合起始的 `assistant<|message|>` 占位符。
const TOKENS_REPLY_PRIMING: usize = 3;

impl TiktokenTokenizer {
    /// 构造 `cl100k_base` 分词器（gpt-4 / gpt-3.5 系列）。
    ///
    /// # Errors
    /// 当 `tiktoken_rs::cl100k_base()` 内部加载词表失败时返回错误描述。
    pub fn new_cl100k() -> Result<Self, String> {
        let bpe = tiktoken_rs::cl100k_base().map_err(|e| e.to_string())?;
        Ok(Self {
            bpe,
            kind: TiktokenKind::Cl100k,
        })
    }

    /// 构造 `o200k_base` 分词器（gpt-4o 系列）。
    ///
    /// # Errors
    /// 当 `tiktoken_rs::o200k_base()` 内部加载词表失败时返回错误描述。
    pub fn new_o200k() -> Result<Self, String> {
        let bpe = tiktoken_rs::o200k_base().map_err(|e| e.to_string())?;
        Ok(Self {
            bpe,
            kind: TiktokenKind::O200k,
        })
    }

    /// 根据 model 名称选择对应分词器。
    ///
    /// 选择规则：
    /// - `gpt-4o` / `gpt-4o-mini` / `o1` / `o3` 系列 → `o200k_base`
    /// - 其它默认 → `cl100k_base`
    ///
    /// # Errors
    /// 词表加载失败时返回错误描述。
    pub fn new_for_model(model: &str) -> Result<Self, String> {
        let lower = model.to_ascii_lowercase();
        let uses_o200k =
            lower.starts_with("gpt-4o") || lower.starts_with("o1") || lower.starts_with("o3");
        if uses_o200k {
            Self::new_o200k()
        } else {
            Self::new_cl100k()
        }
    }

    /// 返回分词器种类。
    #[must_use]
    pub fn kind(&self) -> TiktokenKind {
        self.kind
    }

    /// 编码文本并返回 token 数（不含特殊 token）。
    fn count_text(&self, text: &str) -> usize {
        self.bpe.encode_ordinary(text).len()
    }
}

impl Tokenizer for TiktokenTokenizer {
    fn count(&self, text: &str) -> usize {
        self.count_text(text)
    }

    fn count_messages(&self, msgs: &[Message]) -> usize {
        let mut total: usize = 0;
        for m in msgs {
            total += TOKENS_PER_MESSAGE + TOKENS_PER_NAME;
            total += self.count_text(role_str(&m.role));
            total += self.count_text(&m.text());
            for tc in &m.tool_calls {
                total += self.count_text(&tc.name);
                total += self.count_text(&tc.input.to_string());
            }
        }
        total += TOKENS_REPLY_PRIMING;
        total
    }

    fn id(&self) -> &str {
        self.kind.as_str()
    }
}

/// 返回 role 的小写字符串表示（与 `OpenAI` wire format 一致）。
fn role_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}
