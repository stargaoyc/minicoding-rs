//! 任务数据模型：`Task` / `TaskStatus`（见 `design.md` §18、`api.md` §10.1）。
//!
//! 任务管理工具（`task.create`/`task.update`/`task.list`）的数据模型。状态机
//! `Pending → InProgress → Completed`/`Cancelled` 单向流转，不可跳跃（C-31）。
//! `task_id` 由 Runtime 生成（ULID），LLM 不得伪造。

use crate::model::ToolError;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TaskStatus {
    /// 状态的字符串标签（用于错误消息，与 serde 序列化无关）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    /// 校验状态迁移是否合法（C-31：不可跳跃、不可回退）。
    ///
    /// 合法迁移：`Pending → InProgress`、`InProgress → Completed`、
    /// `InProgress → Cancelled`。其余均非法。
    #[must_use]
    pub fn can_transition_to(self, target: TaskStatus) -> bool {
        matches!(
            (self, target),
            (TaskStatus::Pending, TaskStatus::InProgress)
                | (
                    TaskStatus::InProgress,
                    TaskStatus::Completed | TaskStatus::Cancelled
                )
        )
    }
}

/// 任务（由 `task.create` 创建，`task.update` 增量更新）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// ULID，由 Runtime 生成（C-31：ID 不可由 LLM 伪造）。
    pub id: String,
    /// 任务描述。
    pub content: String,
    pub status: TaskStatus,
    /// `Completed`/`Cancelled` 时必填（实际完成内容/证据）。
    pub summary: Option<String>,
    /// 本任务阻塞的其他 `task_id` 列表。
    pub blocks: Vec<String>,
    /// 阻塞本任务的 `task_id` 列表。
    pub blocked_by: Vec<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl Task {
    /// 创建新任务（`Pending` 状态，ULID 与时间戳由 Runtime 生成）。
    #[must_use]
    pub fn new(content: String) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            id: ulid::Ulid::new().to_string(),
            content,
            status: TaskStatus::Pending,
            summary: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// 刷新 `updated_at` 为当前时间（由组合层在提交增量更新后调用）。
    pub fn touch(&mut self) {
        self.updated_at = OffsetDateTime::now_utc();
    }

    /// 设置状态，校验迁移合法性（C-31）。
    ///
    /// # Errors
    /// 非法迁移返回 `ToolError::InvalidStateTransition`。
    pub fn set_status(&mut self, target: TaskStatus) -> Result<(), ToolError> {
        if !self.status.can_transition_to(target) {
            return Err(ToolError::InvalidStateTransition(format!(
                "cannot transition from {} to {}",
                self.status.as_str(),
                target.as_str()
            )));
        }
        self.status = target;
        self.touch();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_to_in_progress_ok() {
        assert!(TaskStatus::Pending.can_transition_to(TaskStatus::InProgress));
    }

    #[test]
    fn in_progress_to_terminal_ok() {
        assert!(TaskStatus::InProgress.can_transition_to(TaskStatus::Completed));
        assert!(TaskStatus::InProgress.can_transition_to(TaskStatus::Cancelled));
    }

    #[test]
    fn skip_in_progress_rejected() {
        assert!(!TaskStatus::Pending.can_transition_to(TaskStatus::Completed));
        assert!(!TaskStatus::Pending.can_transition_to(TaskStatus::Cancelled));
    }

    #[test]
    fn backward_transition_rejected() {
        assert!(!TaskStatus::Completed.can_transition_to(TaskStatus::InProgress));
        assert!(!TaskStatus::Completed.can_transition_to(TaskStatus::Pending));
        assert!(!TaskStatus::Cancelled.can_transition_to(TaskStatus::InProgress));
        assert!(!TaskStatus::InProgress.can_transition_to(TaskStatus::Pending));
    }

    #[test]
    fn set_status_rejects_backward() {
        let mut task = Task::new("demo".to_string());
        assert!(task.set_status(TaskStatus::InProgress).is_ok());
        assert!(task.set_status(TaskStatus::Completed).is_ok());
        // 已 Completed，回退失败
        assert!(task.set_status(TaskStatus::InProgress).is_err());
    }

    #[test]
    fn new_task_is_pending_with_ulid() {
        let task = Task::new("hello".to_string());
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(!task.id.is_empty());
        // ULID 长度 26
        assert_eq!(task.id.len(), 26);
    }
}
