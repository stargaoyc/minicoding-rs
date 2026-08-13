//! Workspace DTO（W-11 项目工作区，HTTP 端点响应类型）。
//!
//! 对应 `docs/design.md` §26.9 的端点契约：
//!
//! ```text
//! GET  /sessions/{id}/workspace/root   → WorkspaceRoot
//! GET  /sessions/{id}/workspace/list   → WorkspaceListResponse
//! GET  /sessions/{id}/workspace/read   → WorkspaceReadResponse
//! GET  /sessions/{id}/workspace/diff   → WorkspaceDiffResponse
//! POST /sessions/{id}/workspace        → WorkspaceSwitchResponse（切换需审批）
//! ```
//!
//! 这些类型独立于 `Command`/`Response`（HTTP 端点不走 JSON-RPC），但通过
//! `ts_rs` 导出到前端，与 JSON-RPC DTO 共用生成物（AGENTS.md §8.4）。

use serde::{Deserialize, Serialize};

/// 工作区根目录信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct WorkspaceRoot {
    /// 绝对路径。
    pub path: String,
    /// 目录名（展示用）。
    pub name: String,
}

/// 目录列表单条条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct WorkspaceListEntry {
    /// 文件/目录名（不含路径）。
    pub name: String,
    /// `"file"` / `"dir"`。
    pub kind: String,
    /// 文件大小（字节，目录为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// `GET /workspace/list` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct WorkspaceListResponse {
    /// 请求的目录（绝对路径）。
    pub path: String,
    /// 单层条目（已应用 ignore 列表与隐藏文件过滤）。
    pub entries: Vec<WorkspaceListEntry>,
}

/// `GET /workspace/read` 响应（文件内容）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct WorkspaceReadResponse {
    /// 文件内容（≤ 64 KiB，超出截断，C-07 输出上限）。
    pub content: String,
    /// 文件大小（字节）。
    pub size: u64,
    /// 是否被截断。
    pub truncated: bool,
}

/// 单个文件的改动（diff 视图用，源自 `FileChangeJournal`，见 `design.md` §17.2）。
///
/// 与 `minicoding_core::journal::FileChange` 的四种变体一一对应，用
/// `kind` 标签序列化（前端据此渲染增删对比）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceFileChange {
    /// 整文件写入（`before` 为 `None` 表示新建）。
    Written {
        path: String,
        before: Option<String>,
        after: String,
    },
    /// 局部编辑（保留前后全文）。
    Edited {
        path: String,
        before: String,
        after: String,
    },
    /// 删除（`content` 为删除前内容）。
    Deleted { path: String, content: String },
    /// 创建新文件。
    Created { path: String, content: String },
}

/// diff 单条记录（一次操作批次的全部文件改动）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct WorkspaceDiffEntry {
    /// 触发该批改动的用户消息 id。
    pub op_id: String,
    /// 触发消息前 80 字（展示用）。
    pub prompt_snippet: String,
    /// 该批次内的所有文件改动。
    pub files: Vec<WorkspaceFileChange>,
}

/// `GET /workspace/diff` 响应（会话内全部文件改动历史）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct WorkspaceDiffResponse {
    pub entries: Vec<WorkspaceDiffEntry>,
}

/// `POST /workspace` 切换响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct WorkspaceSwitchResponse {
    /// 切换是否成功（`false` = 用户拒绝审批）。
    pub switched: bool,
    /// 当前工作目录（切换后）。
    pub path: String,
}
