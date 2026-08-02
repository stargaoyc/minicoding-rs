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

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use crate::task::InMemoryTaskStore;
    use minicoding_core::model::{SideEffect, ToolContent, ToolError};
    use minicoding_core::tool::{Tool, ToolContext};
    use serde_json::json;
    use std::sync::Arc;

    fn make_store() -> Arc<dyn TaskStore> {
        Arc::new(InMemoryTaskStore::new())
    }

    fn make_ctx() -> ToolContext {
        ToolContext::new("/tmp/proj".into(), "test".to_string())
    }

    #[tokio::test]
    async fn update_valid_transition_returns_updated_task() {
        let store = make_store();
        let task = store.create("do thing".to_string()).await.expect("create");
        let tool = TaskUpdate::new(store);
        let input = json!({"task_id": task.id, "status": "inprogress"});
        let result = tool.execute(input, &make_ctx()).await.expect("execute ok");
        assert!(!result.is_error);
        let ToolContent::Json(value) = result.content else {
            panic!("expected json content");
        };
        assert_eq!(value["id"], task.id);
        assert_eq!(value["status"], "inprogress");
    }

    #[tokio::test]
    async fn update_nonexistent_task_returns_not_found() {
        let tool = TaskUpdate::new(make_store());
        let input = json!({"task_id": "nonexistent", "status": "inprogress"});
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_invalid_transition_returns_error() {
        let store = make_store();
        let task = store.create("do thing".to_string()).await.expect("create");
        let tool = TaskUpdate::new(store);
        // Pending → Completed 是跳跃（须先经 InProgress），非法（C-31）
        let input = json!({"task_id": task.id, "status": "completed", "summary": "done"});
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidStateTransition(_)));
    }

    #[tokio::test]
    async fn update_terminal_status_requires_summary() {
        let store = make_store();
        let task = store.create("do thing".to_string()).await.expect("create");
        // 先迁到 InProgress
        store
            .update(
                task.id.clone(),
                TaskPatch {
                    status: Some(TaskStatus::InProgress),
                    ..Default::default()
                },
            )
            .await
            .expect("transition to inprogress");
        let tool = TaskUpdate::new(store);
        // Completed 缺 summary → 拒绝
        let input = json!({"task_id": task.id, "status": "completed"});
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn update_side_effect_is_none() {
        let tool = TaskUpdate::new(make_store());
        assert_eq!(tool.side_effect(), SideEffect::None);
    }

    #[test]
    fn update_schema_name_is_task_update() {
        let tool = TaskUpdate::new(make_store());
        assert_eq!(tool.schema().name, "task.update");
    }
}
