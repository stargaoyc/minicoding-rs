//! 消息模型：`Message` / `Role` / `ContentBlock` / `MessageMeta`。
//!
//! 与 `OpenAI` / `Anthropic` 消息格式兼容，跨 provider 共享。所有类型支持 `serde`
//! 序列化，用于 JSONL 持久化与跨进程协议。

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 消息角色。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 消息内容块。一条消息可含多个块（如文本 + 工具调用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
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
        /// 工具执行元数据（M-09：含 `sandbox_denied` 结构化拒绝信息；wire 兼容旧数据）。
        #[serde(default)]
        metadata: super::ToolResultMeta,
    },
}

/// 消息来源（用于 `MessageMeta` 审计与 `OTel` span）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct MessageMeta {
    /// 该消息的 token 数（LLM 返回或启发式估算）。
    pub tokens: Option<usize>,
    /// 是否固定（压缩时不裁剪）。
    pub pinned: bool,
    /// 是否已被摘要替换。
    pub summarized: bool,
    /// 消息来源。
    pub source: MessageSource,
    /// 本消息替代了事件 seq 区间 `[from_seq, to_seq]`（压缩追溯，R-02/M-07）。
    ///
    /// 压缩摘要/合并消息携带被替换消息的 seq 区间，审计可追溯"这轮压缩掉了
    /// 什么"（`AuditKind::Compress`）。`None` = 非压缩产物。wire 兼容：
    /// 旧数据无此字段，反序列化时默认 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressed_range: Option<CompressedRange>,
}

/// 压缩追溯区间（M-07，R-02）：本消息替代的事件 seq 区间。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct CompressedRange {
    /// 被替代区间起始事件 seq（含）。
    pub from_seq: u64,
    /// 被替代区间结束事件 seq（含）。
    pub to_seq: u64,
    /// 被替代消息的 token 总量（压缩掉的 token）。
    pub dropped_tokens: usize,
}

/// 一条对话消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub tool_calls: Vec<super::ToolCall>,
    /// `role=Tool` 时指向触发它的 call id。
    pub tool_call_id: Option<super::ToolCallId>,
    /// RFC3339 时间戳（`time::OffsetDateTime` 序列化为 string，见 `design.md` §25.4）。
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
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
            content: vec![ContentBlock::Text { text: text.into() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: OffsetDateTime::now_utc(),
            metadata: MessageMeta {
                source: MessageSource::User,
                compressed_range: None,
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
            content: vec![ContentBlock::Text { text: text.into() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: OffsetDateTime::now_utc(),
            metadata: MessageMeta {
                source: MessageSource::User,
                compressed_range: None,
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
            content: vec![ContentBlock::Text { text: text.into() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: OffsetDateTime::now_utc(),
            metadata: MessageMeta {
                source: MessageSource::Llm,
                compressed_range: None,
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
                if let ContentBlock::Text { text: t } = b {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
