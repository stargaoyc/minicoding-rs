//! `task.create`：创建任务（`Pending` 状态），返回 `task_id`。

use crate::task::TaskStore;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool, ToolContext, ToolOutputSchema};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 创建任务的工具（`SideEffect::None`）。
///
/// `task_id` 由 Runtime 生成（ULID），LLM 不可伪造（C-31）。
pub struct TaskCreate {
    schema: ToolSchema,
    output_schema: ToolOutputSchema,
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
        // R-05（M-11）：声明输出 JSON 形态（task_id + status）。
        let output_schema = ToolOutputSchema {
            schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string"},
                    "status": {"type": "string"}
                },
                "required": ["task_id", "status"]
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

    /// 输出 JSON Schema（R-05，M-11）：task_id + status。
    fn output_schema(&self) -> Option<&ToolOutputSchema> {
        Some(&self.output_schema)
    }

    /// 渲染意图（R-05，M-11）：结构化 JSON 直出。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        RenderIntent::default_for(result)
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
    async fn create_valid_input_returns_task_id_and_pending() {
        let tool = TaskCreate::new(make_store());
        let input = json!({"content": "做某事"});
        let result = tool.execute(input, &make_ctx()).await.expect("execute ok");
        assert!(!result.is_error);
        let ToolContent::Json(value) = result.content else {
            panic!("expected json content");
        };
        assert!(value.get("task_id").is_some());
        assert_eq!(value["status"], "pending");
    }

    #[tokio::test]
    async fn create_missing_content_returns_invalid_input() {
        let tool = TaskCreate::new(make_store());
        let input = json!({});
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn create_empty_content_rejected_by_store() {
        let tool = TaskCreate::new(make_store());
        let input = json!({"content": "   "});
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn create_side_effect_is_none() {
        let tool = TaskCreate::new(make_store());
        assert_eq!(tool.side_effect(), SideEffect::None);
    }

    #[test]
    fn create_schema_name_is_task_create() {
        let tool = TaskCreate::new(make_store());
        assert_eq!(tool.schema().name, "task.create");
    }

    #[test]
    fn create_declares_output_schema() {
        // R-05（M-11）：task.create 声明输出 JSON 形态（task_id + status）。
        let tool = TaskCreate::new(make_store());
        let schema = tool.output_schema().expect("output schema");
        assert_eq!(schema.schema["type"], "object");
        assert_eq!(schema.schema["properties"]["task_id"]["type"], "string");
        assert!(
            schema.schema["required"]
                .as_array()
                .expect("required")
                .iter()
                .any(|v| v == "task_id")
        );
    }

    #[test]
    fn create_render_output_defaults_to_json() {
        let tool = TaskCreate::new(make_store());
        let result = ToolResult::ok_json(json!({"task_id": "x", "status": "pending"}));
        match tool.render_output(&result) {
            RenderIntent::Json { .. } => {}
            other => panic!("expected Json, got {other:?}"),
        }
    }
}
