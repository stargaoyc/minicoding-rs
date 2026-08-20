//! `plan.list`：列出当前 Plan 模式状态（M-11 新增，见 `design.md` §16.4）。
//!
//! 输出当前 `PermissionMode` 与 `plan.exit` 缓存的预批准命令清单
//! （`PlanModeController::snapshot`）。`SideEffect::None`，可穿透 Plan 模式
//! 硬门（`is_read_only() == true`，C-25）——Plan 阶段模型可随时查询当前状态。

use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::policy::{PlanModeController, PlanModeSnapshot};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool, ToolContext, ToolOutputSchema};
use serde_json::json;
use std::sync::Arc;

/// 列出 Plan 模式状态的只读工具。
pub struct PlanList {
    schema: ToolSchema,
    output_schema: ToolOutputSchema,
    controller: Arc<dyn PlanModeController>,
}

impl PlanList {
    /// 创建 `plan.list` 工具实例，共享 `controller`。
    #[must_use]
    pub fn new(controller: Arc<dyn PlanModeController>) -> Self {
        let schema = ToolSchema {
            name: "plan.list".to_string(),
            description: "列出当前 Plan 模式状态：权限模式与预批准命令清单（plan.exit 缓存，\
执行期命中即 Allow）。只读，可在 Plan 模式硬门内调用（C-25）。"
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        };
        let output_schema = ToolOutputSchema {
            schema: json!({
                "type": "object",
                "properties": {
                    "mode": {"type": "string"},
                    "allowed_prompts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": {"type": "string"},
                                "prompt": {"type": "string"}
                            }
                        }
                    }
                },
                "required": ["mode"]
            }),
        };
        Self {
            schema,
            output_schema,
            controller,
        }
    }
}

impl Tool for PlanList {
    fn name(&self) -> &'static str {
        "plan.list"
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    /// 只读：穿透 Plan 模式硬门（C-25），Plan 阶段可查询。
    fn is_read_only(&self) -> bool {
        true
    }

    fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let controller = self.controller.clone();
        Box::pin(async move {
            let snap: PlanModeSnapshot = controller.snapshot().await;
            let prompts: Vec<serde_json::Value> = snap
                .allowed_prompts
                .iter()
                .map(|p| json!({ "tool": p.tool, "prompt": p.prompt }))
                .collect();
            Ok(ToolResult::ok_json(json!({
                "mode": format!("{:?}", snap.mode).to_lowercase(),
                "allowed_prompts": prompts,
            })))
        })
    }

    /// 输出 JSON Schema（R-05，M-11）：`mode` + `allowed_prompts`。
    fn output_schema(&self) -> Option<&ToolOutputSchema> {
        Some(&self.output_schema)
    }

    /// 渲染意图（R-05，M-11）：allowed_prompts 非空 → 表格（tool/prompt）；
    /// 为空 → JSON 直出。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        let minicoding_core::model::ToolContent::Json(value) = &result.content else {
            return RenderIntent::default_for(result);
        };
        let mut rows = Vec::new();
        if let Some(prompts) = value
            .get("allowed_prompts")
            .and_then(serde_json::Value::as_array)
        {
            for p in prompts {
                rows.push(vec![
                    p.get("tool")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    p.get("prompt")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ]);
            }
        }
        if rows.is_empty() {
            return RenderIntent::Json {
                value: value.clone(),
            };
        }
        RenderIntent::Table {
            headers: vec!["tool".into(), "prompt".into()],
            rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::{SideEffect, ToolContent};
    use minicoding_core::policy::{PermissionMode, PlanModeSnapshot, PreApprovedPrompt};
    use minicoding_core::tool::Tool;
    use tokio::sync::RwLock;

    /// 测试用 controller：直接持有 `RwLock<PlanModeSnapshot>`。
    struct TestController {
        state: Arc<RwLock<PlanModeSnapshot>>,
    }

    impl PlanModeController for TestController {
        fn snapshot(&self) -> BoxFuture<'_, PlanModeSnapshot> {
            let state = self.state.clone();
            Box::pin(async move { state.read().await.clone() })
        }

        fn exit_plan(
            &self,
            _allowed_prompts: Vec<PreApprovedPrompt>,
            _target_mode: PermissionMode,
        ) -> BoxFuture<'_, Result<(), minicoding_core::model::PolicyError>> {
            Box::pin(async move { Ok(()) })
        }

        fn set_mode(&self, _mode: PermissionMode) -> BoxFuture<'_, ()> {
            Box::pin(async move {})
        }
    }

    fn make_controller(mode: PermissionMode) -> Arc<TestController> {
        Arc::new(TestController {
            state: Arc::new(RwLock::new(PlanModeSnapshot {
                mode,
                allowed_prompts: Vec::new(),
            })),
        })
    }

    fn make_ctx() -> ToolContext {
        ToolContext::new("/tmp/proj".into(), "test".to_string())
    }

    #[tokio::test]
    async fn list_reports_current_mode_and_prompts() {
        let controller = make_controller(PermissionMode::Plan);
        {
            let mut state = controller.state.write().await;
            state.allowed_prompts = vec![PreApprovedPrompt {
                tool: "shell.run".to_string(),
                prompt: "cargo build".to_string(),
            }];
        }
        let tool = PlanList::new(controller.clone());
        let result = tool.execute(json!({}), &make_ctx()).await.unwrap();
        assert!(!result.is_error);
        let ToolContent::Json(value) = result.content else {
            panic!("expected json content");
        };
        assert_eq!(value["mode"], "plan");
        assert_eq!(value["allowed_prompts"][0]["tool"], "shell.run");
        assert_eq!(value["allowed_prompts"][0]["prompt"], "cargo build");
    }

    #[tokio::test]
    async fn list_works_in_default_mode_too() {
        let tool = PlanList::new(make_controller(PermissionMode::Default));
        let result = tool.execute(json!({}), &make_ctx()).await.unwrap();
        let ToolContent::Json(value) = result.content else {
            panic!("expected json content");
        };
        assert_eq!(value["mode"], "default");
        let prompts = value["allowed_prompts"].as_array().expect("array");
        assert!(prompts.is_empty(), "expected empty: prompts");
    }

    #[test]
    fn list_is_read_only_and_no_side_effect() {
        let tool = PlanList::new(make_controller(PermissionMode::Plan));
        assert_eq!(tool.side_effect(), SideEffect::None);
        assert!(tool.is_read_only());
    }

    #[test]
    fn list_schema_and_output_schema() {
        let tool = PlanList::new(make_controller(PermissionMode::Plan));
        assert_eq!(tool.schema().name, "plan.list");
        let out = tool.output_schema().expect("output schema");
        assert_eq!(out.schema["type"], "object");
    }

    #[tokio::test]
    async fn render_output_projects_prompts_to_table() {
        let controller = make_controller(PermissionMode::Plan);
        {
            let mut state = controller.state.write().await;
            state.allowed_prompts = vec![
                PreApprovedPrompt {
                    tool: "shell.run".to_string(),
                    prompt: "cargo build".to_string(),
                },
                PreApprovedPrompt {
                    tool: "shell.run".to_string(),
                    prompt: "cargo test".to_string(),
                },
            ];
        }
        let tool = PlanList::new(controller);
        let result = tool.execute(json!({}), &make_ctx()).await.unwrap();
        match tool.render_output(&result) {
            RenderIntent::Table { headers, rows } => {
                assert_eq!(headers, vec!["tool", "prompt"]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][1], "cargo build");
                assert_eq!(rows[1][1], "cargo test");
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }
}
