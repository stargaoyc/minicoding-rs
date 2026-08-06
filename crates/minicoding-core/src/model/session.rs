//! 会话模型：`Session` / `SessionId` / `StopReason` / `TurnOutcome` / `UserInput`。

use crate::model::{Message, Task};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 会话 ID（ULID 字符串）。
pub type SessionId = String;

/// 会话元数据（轻量列出，不加载消息）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct SessionMeta {
    pub id: SessionId,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub message_count: usize,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub last_message_at: OffsetDateTime,
    /// 任务列表（跨压缩保留，见 `design.md` §18.5、C-31）。
    ///
    /// 任务列表存 `SessionMeta` 而非 `messages`，不受上下文压缩管道影响；
    /// Runtime 负责在 `TaskStore` 与该字段间同步状态。
    #[serde(default)]
    pub tasks: Vec<Task>,
}

/// 会话（运行时镜像，与 storage 一致）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct Session {
    pub id: SessionId,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub workdir: Utf8PathBuf,
    /// 配置 hash，用于 resume 时校验一致性。
    pub config_hash: u64,
    pub messages: Vec<Message>,
}

impl Session {
    /// 创建新会话（空消息）。
    #[must_use]
    pub fn new(workdir: Utf8PathBuf, config_hash: u64) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            created_at: OffsetDateTime::now_utc(),
            workdir,
            config_hash,
            messages: Vec::new(),
        }
    }
}

/// LLM 停止原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Stopped,
    Interrupted,
}

/// 一轮对话的结果。
#[derive(Debug)]
pub enum TurnOutcome {
    /// 正常完成（含最终消息）。
    Finished(Message),
    /// 被中断（含已生成的部分消息）。
    Interrupted(Message),
    /// 失败。
    Failed(crate::model::RuntimeError),
}

/// 用户输入提示类型（影响上下文构建）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(rename_all = "snake_case")]
pub enum ContextHint {
    /// 普通提问。
    #[default]
    Question,
    /// 代码编辑任务。
    Edit,
    /// 探索/搜索任务。
    Explore,
}

/// 附件（文件或图片）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Attachment {
    File(#[cfg_attr(feature = "ts", ts(type = "string"))] Utf8PathBuf),
    Image { data: Vec<u8>, mime: String },
}

/// 用户输入。
#[derive(Debug, Clone)]
pub struct UserInput {
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub context_hint: ContextHint,
}

impl UserInput {
    /// 从纯文本创建。
    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            attachments: Vec::new(),
            context_hint: ContextHint::Question,
        }
    }
}
