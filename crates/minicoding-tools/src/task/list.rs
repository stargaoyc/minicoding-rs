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

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use crate::task::InMemoryTaskStore;
    use minicoding_core::model::{SideEffect, ToolContent};
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
    async fn list_empty_store_returns_empty() {
        let tool = TaskList::new(make_store());
        let result = tool
            .execute(json!({}), &make_ctx())
            .await
            .expect("execute ok");
        assert!(!result.is_error);
        let ToolContent::Json(value) = result.content else {
            panic!("expected json content");
        };
        let tasks = value["tasks"].as_array().expect("tasks array");
        assert!(tasks.is_empty(), "expected empty: tasks");
    }

    #[tokio::test]
    async fn list_returns_all_tasks() {
        let store = make_store();
        store.create("task a".to_string()).await.expect("create a");
        store.create("task b".to_string()).await.expect("create b");
        let tool = TaskList::new(store);
        let result = tool
            .execute(json!({}), &make_ctx())
            .await
            .expect("execute ok");
        assert!(!result.is_error);
        let ToolContent::Json(value) = result.content else {
            panic!("expected json content");
        };
        let tasks = value["tasks"].as_array().expect("tasks array");
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn list_side_effect_is_none() {
        let tool = TaskList::new(make_store());
        assert_eq!(tool.side_effect(), SideEffect::None);
    }

    #[test]
    fn list_schema_name_is_task_list() {
        let tool = TaskList::new(make_store());
        assert_eq!(tool.schema().name, "task.list");
    }
}
