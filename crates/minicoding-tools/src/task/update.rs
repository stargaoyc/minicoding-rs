//! `task.update`：按 `task_id` 增量更新任务（状态机 + 依赖边，见 C-31）。

use crate::task::{TaskPatch, TaskStore};
use minicoding_core::model::{SideEffect, TaskStatus, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 增量更新任务的工具（`SideEffect::None`）。
///
/// 仅更新非 `None` 字段；`add_blocks`/`add_blocked_by` 增量添加依赖边（幂等）。
/// 状态迁移须合法（不可跳跃、不可回退）；`Completed`/`Cancelled` 必填 `summary`；
/// 依赖图不可成环（C-31）。
pub struct TaskUpdate {
    schema: ToolSchema,
    store: Arc<dyn TaskStore>,
}

impl TaskUpdate {
    /// 创建 `task.update` 工具实例，共享 `store`。
    #[must_use]
    pub fn new(store: Arc<dyn TaskStore>) -> Self {
        let schema = ToolSchema {
            name: "task.update".to_string(),
            description:
                "按 task_id 增量更新任务。仅更新非 None 字段；状态机 Pending→InProgress→Completed/Cancelled 单向不可跳跃（C-31）；Completed/Cancelled 必填 summary；add_blocks/add_blocked_by 增量添加依赖边（幂等），不可成环。"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "要更新的任务 ID（须为 task.create 返回的已注册 ID）。"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "inprogress", "completed", "cancelled"],
                        "description": "目标状态（须为合法迁移）。"
                    },
                    "summary": {
                        "type": "string",
                        "description": "完成/取消时的实际内容或证据（Completed/Cancelled 必填）。"
                    },
                    "add_blocks": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "增量添加：本任务阻塞的 task_id 列表（幂等）。"
                    },
                    "add_blocked_by": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "增量添加：阻塞本任务的 task_id 列表（幂等）。"
                    }
                },
                "required": ["task_id"]
            }),
        };
        Self { schema, store }
    }
}

#[derive(Deserialize)]
struct UpdateInput {
    task_id: String,
    status: Option<TaskStatus>,
    summary: Option<String>,
    add_blocks: Option<Vec<String>>,
    add_blocked_by: Option<Vec<String>>,
}

impl Tool for TaskUpdate {
    fn name(&self) -> &'static str {
        "task.update"
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let store = self.store.clone();
        Box::pin(async move {
            let args: UpdateInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let patch = TaskPatch {
                status: args.status,
                summary: args.summary,
                add_blocks: args.add_blocks,
                add_blocked_by: args.add_blocked_by,
            };
            let task = store.update(args.task_id, patch).await?;
            Ok(ToolResult::ok_json(serde_json::to_value(&task).map_err(
                |e| ToolError::InvalidInput(format!("serialize task: {e}")),
            )?))
        })
    }
}
