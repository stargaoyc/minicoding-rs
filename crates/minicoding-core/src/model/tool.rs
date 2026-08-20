//! 工具模型：`ToolCall` / `ToolSchema` / `ToolResult` / `SideEffect`。
//!
//! 工具调用的请求与响应结构，跨 provider 共享。`SideEffect` 用于调度策略
//! （无副作用并行，有副作用串行，见 `design.md` §2.3）。

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 工具调用 ID 类型（ULID 或 provider 返回的 id）。
pub type ToolCallId = String;

/// 工具副作用分类（决定调度策略与权限决策）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    /// 只读：无副作用（`fs.read`/`fs.list`/`fs.glob`/`fs.grep`）。
    None,
    /// 文件写入：`fs.write`/`fs.edit`/`fs.delete`。
    FileWrite,
    /// 命令执行：`shell.run`。
    Command,
    /// 网络访问：`web.fetch`/`web.search`。
    Network,
}

/// 工具调用请求（LLM 产出）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    /// 工具输入参数（JSON）。
    pub input: serde_json::Value,
}

/// 工具 schema（注册时声明，供 LLM 调用参考）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema 描述输入参数。
    pub input_schema: serde_json::Value,
}

/// 工具返回内容（支持文本/JSON/图片/混合）。
///
/// 序列化用 adjacent tagging（`{"type":"text","content":"..."}`）：internal tag 无法
/// 序列化 newtype 变体（`Text(String)`/`Json(Value)` 内容不是 map/struct），会导致
/// 工具结果落盘序列化失败。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ToolContent {
    Text(String),
    Json(serde_json::Value),
    Image { mime: String, data: Vec<u8> },
    Mixed(Vec<ToolContent>),
}

impl ToolContent {
    /// 从字符串创建文本内容。
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
}

/// 沙箱拒绝详情（M-09 结构化透传，wire 可选字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct SandboxDenyInfo {
    /// 结构化拒绝类型（前端渲染拒绝卡片）。
    pub kind: crate::sandbox::SandboxDenyKind,
    /// 原始错误文本（含 stderr，供审计/诊断）。
    pub detail: String,
}

/// 工具执行结果元数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct ToolResultMeta {
    /// serde 序列化为 `{ secs, nanos }`（`std::time::Duration` 默认 impl）。
    #[cfg_attr(feature = "ts", ts(type = "{ secs: number; nanos: number }"))]
    pub elapsed: Duration,
    pub bytes: usize,
    pub truncated: bool,
    /// 沙箱拒绝结构化信息（M-09；非拒绝结果为 `None`，wire 省略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_denied: Option<SandboxDenyInfo>,
}

/// 工具执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct ToolResult {
    pub content: ToolContent,
    pub is_error: bool,
    pub metadata: ToolResultMeta,
}

impl ToolResult {
    /// 创建成功结果（文本）。
    #[must_use]
    pub fn ok_text(text: impl Into<String>) -> Self {
        Self {
            content: ToolContent::text(text),
            is_error: false,
            metadata: ToolResultMeta::default(),
        }
    }

    /// 创建成功结果（JSON，用于结构化工具输出如 `task.*`）。
    #[must_use]
    pub fn ok_json(value: serde_json::Value) -> Self {
        Self {
            content: ToolContent::Json(value),
            is_error: false,
            metadata: ToolResultMeta::default(),
        }
    }

    /// 创建错误结果（文本）。
    #[must_use]
    pub fn err_text(text: impl Into<String>) -> Self {
        Self {
            content: ToolContent::text(text),
            is_error: true,
            metadata: ToolResultMeta::default(),
        }
    }
}
