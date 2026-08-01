//! `task.list`：列出任务（可按 `status` 过滤）。

use crate::task::TaskStore;
use minicoding_core::model::{SideEffect, TaskStatus, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 列出任务的工具（`SideEffect::None`）。
pub struct TaskList {
    schema: ToolSchema,
    store: Arc<dyn TaskStore>,
}

impl TaskList {
    /// 创建 `task.list` 工具实例，共享 `store`。
    #[must_use]
    pub fn new(store: Arc<dyn TaskStore>) -> Self {
        let schema = ToolSchema {
            name: "task.list".to_string(),
            description: "列出任务（可按 status 过滤）。任务列表跨压缩保留（C-31）。".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["pending", "inprogress", "completed", "cancelled"],
                        "description": "仅返回该状态的任务；省略则返回全部。"
                    }
                }
            }),
        };
        Self { schema, store }
    }
}

#[derive(Deserialize)]
struct ListInput {
    status: Option<TaskStatus>,
}

impl Tool for TaskList {
    fn name(&self) -> &'static str {
        "task.list"
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
            let args: ListInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let tasks = store.list(args.status).await;
            Ok(ToolResult::ok_json(json!({ "tasks": tasks })))
        })
    }
}
