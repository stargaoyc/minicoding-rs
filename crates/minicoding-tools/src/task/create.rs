//! `task.create`：创建任务（`Pending` 状态），返回 `task_id`。

use crate::task::TaskStore;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 创建任务的工具（`SideEffect::None`）。
///
/// `task_id` 由 Runtime 生成（ULID），LLM 不可伪造（C-31）。
pub struct TaskCreate {
    schema: ToolSchema,
    store: Arc<dyn TaskStore>,
}

impl TaskCreate {
    /// 创建 `task.create` 工具实例，共享 `store`。
    #[must_use]
    pub fn new(store: Arc<dyn TaskStore>) -> Self {
        let schema = ToolSchema {
            name: "task.create".to_string(),
            description:
                "创建一个任务（Pending 状态）并返回 task_id。任务 ID 由 Runtime 生成，不可伪造（C-31）。"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "任务描述（非空）。"
                    }
                },
                "required": ["content"]
            }),
        };
        Self { schema, store }
    }
}

#[derive(Deserialize)]
struct CreateInput {
    content: String,
}

impl Tool for TaskCreate {
    fn name(&self) -> &'static str {
        "task.create"
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
            let args: CreateInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let task = store.create(args.content).await?;
            Ok(ToolResult::ok_json(
                json!({ "task_id": task.id, "status": task.status }),
            ))
        })
    }
}
