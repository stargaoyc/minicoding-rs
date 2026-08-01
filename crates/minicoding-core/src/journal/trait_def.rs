//! `Journal` trait 与数据结构定义（见 `api.md` §3.11、`design.md` §17）。

use crate::provider::BoxFuture;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 操作 id（关联触发该批改动的 turn / prompt）。
///
/// 用 `String` 而非 `uuid::Uuid`：允许调用方传入 turn 的用户消息 id 或任意标识，
/// 不强制 UUID 格式（与 `MessageSource` 的 id 同类型）。
pub type OpId = String;

/// 一次 turn 的文件改动集合（`fs.write`/`fs.edit`/`fs.delete` 成功后合并记录）。
///
/// `op_id` 关联该 turn 的用户消息 id——`/undo 1` 撤销"最近一次用户消息触发的所有
/// 文件改动"，符合直觉（见 `design.md` §17.3）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEntry {
    /// 关联触发该批改动的 turn / prompt 的 id。
    pub op_id: OpId,
    /// 记录时间。
    pub ts: OffsetDateTime,
    /// 触发该批改动的用户消息前 80 字（供 `/undo` 预览展示）。
    pub prompt_snippet: String,
    /// 该 turn 内的所有文件改动（按发生顺序）。
    pub files: Vec<FileChange>,
}

/// 单个文件的改动记录。
///
/// `before: None` 表示新建文件；`Deleted.content` 用于撤销时恢复内容
/// （但无法恢复元数据如权限/mtime，见 `design.md` §17.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileChange {
    /// `fs.write` 整文件写入。`before` 为 `None` 表示新建。
    Written {
        path: Utf8PathBuf,
        before: Option<Vec<u8>>,
        after: Vec<u8>,
    },
    /// `fs.edit` 局部编辑（保留 before/after 全文便于冲突检测与恢复）。
    Edited {
        path: Utf8PathBuf,
        before: Vec<u8>,
        after: Vec<u8>,
    },
    /// `fs.delete` 删除（`content` 用于撤销时恢复）。
    Deleted { path: Utf8PathBuf, content: Vec<u8> },
    /// `fs.write` 创建新文件（`before` 为 `None` 的语义糖，撤销时删除文件）。
    Created { path: Utf8PathBuf, content: Vec<u8> },
}

impl FileChange {
    /// 文件路径。
    #[must_use]
    pub fn path(&self) -> &Utf8PathBuf {
        match self {
            Self::Written { path, .. }
            | Self::Edited { path, .. }
            | Self::Deleted { path, .. }
            | Self::Created { path, .. } => path,
        }
    }

    /// 改动后的内容（用于冲突检测：恢复前比对当前文件内容与此值）。
    ///
    /// `Deleted` 返回空 `Vec`（删除后文件应不存在；冲突检测判断文件是否被重建）。
    #[must_use]
    pub fn after_content(&self) -> &[u8] {
        match self {
            Self::Written { after, .. } | Self::Edited { after, .. } => after,
            Self::Created { content, .. } => content,
            // 删除后"after 状态"是文件不存在；用空切片表示，调用方据此判断
            Self::Deleted { .. } => &[],
        }
    }
}

/// `/undo` 结果报告（见 `design.md` §17.4）。
///
/// `failed_files` 记录冲突文件（当前内容与 `after` 不一致，已外部编辑），不强行
/// 覆盖（C-28）。错误以描述字符串存储（`JournalError` 含不可 `Clone` 的
/// `std::io::Error`，报告展示用描述足够）。
#[derive(Debug, Clone, Default)]
pub struct UndoReport {
    /// 实际撤销的 entry 数（可能小于请求的 `steps`，若 journal 不足）。
    pub undone_entries: usize,
    /// 成功恢复的文件路径。
    pub restored_files: Vec<Utf8PathBuf>,
    /// 冲突未恢复的文件路径与错误原因描述（C-28 不强行覆盖）。
    pub failed_files: Vec<(Utf8PathBuf, String)>,
}

/// `/diff` 单条记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffEntry {
    pub op_id: OpId,
    pub prompt_snippet: String,
    pub files: Vec<FileChange>,
}

/// 文件改动事务 trait（`dyn` 兼容，见 `api.md` §3.11）。
///
/// 实现为纯内存数据结构（`FileChangeJournal` 在 `minicoding-journal`），不落盘
/// （C-28：含文件原文，落盘等于多存一份敏感数据）。`undo` 含冲突检测：恢复前
/// 比对当前文件内容与 `after`，不一致记入 `failed_files` 不强行覆盖。
///
/// 恢复路径必须经 `sandbox_path` 校验（C-03/C-28 不可越界恢复）——由实现保证。
pub trait Journal: Send + Sync {
    /// 记录一次 turn 的文件改动（`fs.write`/`edit`/`delete` 成功后调用）。
    ///
    /// # Errors
    /// 仅在实现内部锁中毒等不可恢复场景返回 `Err`；正常情况恒成功（内存追加）。
    fn record(&self, entry: ChangeEntry) -> BoxFuture<'_, Result<(), super::JournalError>>;

    /// 撤销最近 `steps` 次 turn 的文件改动（`/undo`），含冲突检测。
    ///
    /// `steps = 0` 视为 1（撤销最近一次）。`steps` 超过已记录 entry 数时撤销全部
    /// 可撤销项（不返回 `Err`，报告 `undone_entries` 实际值）。冲突文件记入
    /// `failed_files` 不强行覆盖（C-28）。
    ///
    /// # Errors
    /// 恢复路径越界（`PathEscaped`）或 IO 失败时返回 `Err`；冲突文件不返回 `Err`
    /// 而是记入 `failed_files`。
    fn undo(&self, steps: usize) -> BoxFuture<'_, Result<UndoReport, super::JournalError>>;

    /// 列出会话内所有文件变更（`/diff`）。
    ///
    /// # Errors
    /// 仅在实现内部锁中毒时返回 `Err`。
    fn diff(&self) -> BoxFuture<'_, Result<Vec<DiffEntry>, super::JournalError>>;

    /// 回到会话启动时状态（`/new`），清空 journal。
    ///
    /// # Errors
    /// 仅在实现内部锁中毒时返回 `Err`。
    fn reset_to_initial(&self) -> BoxFuture<'_, Result<(), super::JournalError>>;
}
