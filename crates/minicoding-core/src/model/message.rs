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

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    prop_compose! {
        fn arb_message()(
            role in prop_oneof![
                Just(Role::System),
                Just(Role::User),
                Just(Role::Assistant),
                Just(Role::Tool),
            ],
            text in "[a-zA-Z0-9 .,!?-]{0,100}",
            id in "[a-z0-9]{10,26}",
            tool_call_id in proptest::option::of("[a-z0-9]{0,20}"),
            pinned in proptest::bool::ANY,
            summarized in proptest::bool::ANY,
            tokens in proptest::option::of(0usize..10_000),
            secs in 0i64..2_000_000_000i64,
        ) -> Message {
            let source = match role {
                Role::System | Role::User => MessageSource::User,
                Role::Assistant => MessageSource::Llm,
                Role::Tool => MessageSource::Tool,
            };
            Message {
                id,
                role,
                content: vec![ContentBlock::Text { text }],
                tool_calls: Vec::new(),
                tool_call_id,
                created_at: OffsetDateTime::from_unix_timestamp(secs)
                    .expect("secs within valid OffsetDateTime range"),
                metadata: MessageMeta {
                    tokens,
                    pinned,
                    summarized,
                    source,
                },
            }
        }
    }

    proptest! {
        #[test]
        fn message_json_roundtrip(msg in arb_message()) {
            // Message 不派生 PartialEq：序列化→反序列化→再序列化后比较 JSON 字符串，
            // 若两次序列化结果一致则说明 roundtrip 无信息丢失。
            let json = serde_json::to_string(&msg).expect("serialize Message");
            let decoded: Message = serde_json::from_str(&json).expect("deserialize Message");
            let json2 = serde_json::to_string(&decoded).expect("serialize decoded Message");
            prop_assert_eq!(json, json2);
        }

        #[test]
        fn role_json_roundtrip(role in prop_oneof![
            Just(Role::System),
            Just(Role::User),
            Just(Role::Assistant),
            Just(Role::Tool),
        ]) {
            let json = serde_json::to_string(&role).expect("serialize Role");
            let decoded: Role = serde_json::from_str(&json).expect("deserialize Role");
            // Role 派生 PartialEq + Eq
            prop_assert_eq!(role, decoded);
        }
    }
}
