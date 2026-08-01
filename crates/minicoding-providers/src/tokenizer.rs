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

    fn id(&self) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::Message;

    #[test]
    fn kind_as_str_returns_identifier() {
        assert_eq!(TiktokenKind::Cl100k.as_str(), "cl100k");
        assert_eq!(TiktokenKind::O200k.as_str(), "o200k");
    }

    #[test]
    fn debug_format_hides_bpe_vocab() {
        // Debug 输出含结构体名与 kind 字段，但不泄漏 bpe 词表
        let tok = TiktokenTokenizer::new_cl100k().expect("加载 cl100k 词表");
        let s = format!("{tok:?}");
        assert!(s.contains("TiktokenTokenizer"), "Debug 应含结构体名: {s}");
        assert!(s.contains("Cl100k"), "Debug 应含 kind 字段: {s}");
        assert!(!s.contains("CoreBPE"), "Debug 不应含 bpe 词表: {s}");
    }

    #[test]
    fn count_text_empty_returns_zero() {
        let tok = TiktokenTokenizer::new_cl100k().expect("加载 cl100k 词表");
        assert_eq!(tok.count(""), 0);
    }

    #[test]
    fn count_text_english_positive() {
        let tok = TiktokenTokenizer::new_cl100k().expect("加载 cl100k 词表");
        assert!(tok.count("hello world") > 0, "英文文本 token 数应 > 0");
    }

    #[test]
    fn count_grows_with_longer_text() {
        let tok = TiktokenTokenizer::new_cl100k().expect("加载 cl100k 词表");
        let short = tok.count("hi");
        let long = tok.count("hello world, this is a longer sentence for testing");
        assert!(
            long > short,
            "长文本 token 数应更多: short={short}, long={long}"
        );
    }

    #[test]
    fn count_messages_empty_returns_reply_priming_only() {
        // 空消息列表：仅 TOKENS_REPLY_PRIMING = 3
        let tok = TiktokenTokenizer::new_cl100k().expect("加载 cl100k 词表");
        assert_eq!(tok.count_messages(&[]), TOKENS_REPLY_PRIMING);
    }

    #[test]
    fn count_messages_nonempty_positive() {
        let tok = TiktokenTokenizer::new_cl100k().expect("加载 cl100k 词表");
        let msgs = vec![Message::user_text("hello world")];
        let n = tok.count_messages(&msgs);
        assert!(
            n > TOKENS_REPLY_PRIMING,
            "非空消息列表 token 数应超过 priming 开销: {n}"
        );
    }

    #[test]
    fn count_messages_more_messages_more_tokens() {
        let tok = TiktokenTokenizer::new_cl100k().expect("加载 cl100k 词表");
        let one = tok.count_messages(&[Message::user_text("hello")]);
        let two = tok.count_messages(&[
            Message::user_text("hello"),
            Message::assistant_text("hi there"),
        ]);
        assert!(two > one, "更多消息应计更多 token: one={one}, two={two}");
    }

    #[test]
    fn new_for_model_selects_o200k_for_gpt4o() {
        let tok = TiktokenTokenizer::new_for_model("gpt-4o").expect("加载 o200k 词表");
        assert_eq!(tok.kind(), TiktokenKind::O200k);
        assert_eq!(tok.id(), "o200k");
    }

    #[test]
    fn new_for_model_selects_o200k_for_gpt4o_mini() {
        let tok = TiktokenTokenizer::new_for_model("gpt-4o-mini").expect("加载 o200k 词表");
        assert_eq!(tok.kind(), TiktokenKind::O200k);
    }

    #[test]
    fn new_for_model_selects_cl100k_for_gpt4() {
        let tok = TiktokenTokenizer::new_for_model("gpt-4").expect("加载 cl100k 词表");
        assert_eq!(tok.kind(), TiktokenKind::Cl100k);
        assert_eq!(tok.id(), "cl100k");
    }

    #[test]
    fn new_for_model_selects_cl100k_for_gpt35_turbo() {
        let tok = TiktokenTokenizer::new_for_model("gpt-3.5-turbo").expect("加载 cl100k 词表");
        assert_eq!(tok.kind(), TiktokenKind::Cl100k);
    }

    #[test]
    fn new_for_model_selects_o200k_for_o1_and_o3_series() {
        let o1 = TiktokenTokenizer::new_for_model("o1-preview").expect("加载 o200k 词表");
        assert_eq!(o1.kind(), TiktokenKind::O200k);
        let o3 = TiktokenTokenizer::new_for_model("o3-mini").expect("加载 o200k 词表");
        assert_eq!(o3.kind(), TiktokenKind::O200k);
    }

    #[test]
    fn new_for_model_case_insensitive_prefix() {
        // 大写前缀也应匹配（model.to_ascii_lowercase）
        let tok = TiktokenTokenizer::new_for_model("GPT-4O").expect("加载 o200k 词表");
        assert_eq!(tok.kind(), TiktokenKind::O200k);
    }

    #[test]
    fn role_str_matches_all_variants() {
        assert_eq!(role_str(&Role::System), "system");
        assert_eq!(role_str(&Role::User), "user");
        assert_eq!(role_str(&Role::Assistant), "assistant");
        assert_eq!(role_str(&Role::Tool), "tool");
    }
}
