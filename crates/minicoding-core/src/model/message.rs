//! 消息模型：`Message` / `Role` / `ContentBlock` / `MessageMeta`。
//!
//! 与 `OpenAI` / `Anthropic` 消息格式兼容，跨 provider 共享。所有类型支持 `serde`
//! 序列化，用于 JSONL 持久化与跨进程协议。

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 消息角色。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 消息内容块。一条消息可含多个块（如文本 + 工具调用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(String),
    Image {
        mime: String,
        /// base64 编码（传输态），运行态可解为 `Vec<u8>`。
        data: String,
    },
    ToolUse(super::ToolCall),
    ToolResult {
        call_id: super::ToolCallId,
        content: super::ToolContent,
        is_error: bool,
    },
}

/// 消息来源（用于 `MessageMeta` 审计与 `OTel` span）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSource {
    #[default]
    User,
    Llm,
    Tool,
    Subagent,
}

/// 消息元数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageMeta {
    /// 该消息的 token 数（LLM 返回或启发式估算）。
    pub tokens: Option<usize>,
    /// 是否固定（压缩时不裁剪）。
    pub pinned: bool,
    /// 是否已被摘要替换。
    pub summarized: bool,
    /// 消息来源。
    pub source: MessageSource,
}

/// 一条对话消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub tool_calls: Vec<super::ToolCall>,
    /// `role=Tool` 时指向触发它的 call id。
    pub tool_call_id: Option<super::ToolCallId>,
    pub created_at: OffsetDateTime,
    pub metadata: MessageMeta,
}

impl Message {
    /// 创建用户文本消息。
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            role: Role::User,
            content: vec![ContentBlock::Text(text.into())],
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: OffsetDateTime::now_utc(),
            metadata: MessageMeta {
                source: MessageSource::User,
                ..Default::default()
            },
        }
    }

    /// 创建系统文本消息。
    #[must_use]
    pub fn system_text(text: impl Into<String>) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            role: Role::System,
            content: vec![ContentBlock::Text(text.into())],
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: OffsetDateTime::now_utc(),
            metadata: MessageMeta {
                source: MessageSource::User,
                ..Default::default()
            },
        }
    }

    /// 创建助手文本消息。
    #[must_use]
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::Text(text.into())],
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: OffsetDateTime::now_utc(),
            metadata: MessageMeta {
                source: MessageSource::Llm,
                ..Default::default()
            },
        }
    }

    /// 提取所有文本块拼为单个字符串。
    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::Text(t) = b {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
