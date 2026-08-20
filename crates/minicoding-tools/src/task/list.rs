//! `task.list`：列出任务（可按 `status` 过滤）。

use crate::task::TaskStore;
use minicoding_core::model::{SideEffect, TaskStatus, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool, ToolContext, ToolOutputSchema};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 列出任务的工具（`SideEffect::None`）。
pub struct TaskList {
    schema: ToolSchema,
    output_schema: ToolOutputSchema,
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
        // R-05（M-11）：声明输出 JSON 形态（tasks 数组），前端校验后本地渲染。
        let output_schema = ToolOutputSchema {
            schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "content": {"type": "string"},
                                "status": {"type": "string"},
                                "summary": {"type": ["string", "null"]}
                            }
                        }
                    }
                },
                "required": ["tasks"]
            }),
        };
        Self {
            schema,
            output_schema,
            store,
        }
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

    /// 输出 JSON Schema（R-05，M-11）：tasks 数组。
    fn output_schema(&self) -> Option<&ToolOutputSchema> {
        Some(&self.output_schema)
    }

    /// 渲染意图（R-05，M-11）：任务列表 → 表格卡片（id/状态/内容）。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        let minicoding_core::model::ToolContent::Json(value) = &result.content else {
            return RenderIntent::default_for(result);
        };
        let mut rows = Vec::new();
        if let Some(tasks) = value.get("tasks").and_then(serde_json::Value::as_array) {
            for t in tasks {
                rows.push(vec![
                    t.get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    t.get("status")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    t.get("content")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ]);
            }
        }
        // 有任务时渲染为表格；无任务退化为 JSON（保持内容可见）。
        if rows.is_empty() {
            return RenderIntent::Json {
                value: value.clone(),
            };
        }
        RenderIntent::Table {
            headers: vec!["id".into(), "status".into(), "content".into()],
            rows,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use crate::task::{InMemoryTaskStore, TaskPatch};
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

    #[test]
    fn output_schema_declares_tasks_array() {
        let tool = TaskList::new(make_store());
        let schema = tool.output_schema().expect("output schema");
        assert_eq!(schema.schema["type"], "object");
        assert_eq!(schema.schema["properties"]["tasks"]["type"], "array");
    }

    #[tokio::test]
    async fn render_output_projects_tasks_to_table() {
        let store = make_store();
        let _a = store.create("task a".to_string()).await.unwrap();
        let b = store.create("task b".to_string()).await.unwrap();
        store
            .update(
                b.id.clone(),
                TaskPatch {
                    status: Some(TaskStatus::InProgress),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let tool = TaskList::new(store);
        let tasks = tool
            .execute(json!({}), &make_ctx())
            .await
            .expect("execute ok");
        match tool.render_output(&tasks) {
            RenderIntent::Table { headers, rows } => {
                assert_eq!(headers, vec!["id", "status", "content"]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][1], "pending");
                assert_eq!(rows[1][1], "inprogress");
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn render_output_empty_tasks_falls_back_to_json() {
        let tool = TaskList::new(make_store());
        let result = tool
            .execute(json!({}), &make_ctx())
            .await
            .expect("execute ok");
        match tool.render_output(&result) {
            RenderIntent::Json { .. } => {}
            other => panic!("expected Json fallback, got {other:?}"),
        }
    }
}
