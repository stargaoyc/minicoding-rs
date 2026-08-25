//! `shell.output`：非阻塞读取后台 shell 已累积的输出（T-M8-5）。
//!
//! 返回 stdout/stderr 快照 + 是否已退出 + 退出码。多次调用返回增量累积内容
//! （非增量 diff，调用方需自行 diff 上次快照）。

use super::background::BackgroundShellStore;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool, ToolOutputSchema};
use std::sync::Arc;

/// `shell.output` 工具：非阻塞读取后台 shell 输出。
pub struct ShellOutput {
    schema: ToolSchema,
    output_schema: ToolOutputSchema,
    store: Arc<dyn BackgroundShellStore>,
}

impl ShellOutput {
    /// 创建工具实例，注入共享 [`BackgroundShellStore`]。
    #[must_use]
    pub fn new(store: Arc<dyn BackgroundShellStore>) -> Self {
        let schema = ToolSchema {
            name: "shell.output".into(),
            description: "非阻塞读取后台 shell 已累积的 stdout/stderr + 退出状态。\
                          多次调用返回累积快照（非增量）。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "shell_id": {
                        "type": "string",
                        "description": "shell.background 返回的 shell_id"
                    }
                },
                "required": ["shell_id"]
            }),
        };
        // R-05（M-11）：声明输出 JSON 形态（stdout/stderr 快照 + 退出状态）。
        let output_schema = ToolOutputSchema {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "stdout": {"type": "string"},
                    "stderr": {"type": "string"},
                    "exited": {"type": "boolean"},
                    "exit_code": {"type": ["integer", "null"]}
                },
                "required": ["stdout", "stderr", "exited"]
            }),
        };
        Self {
            schema,
            output_schema,
            store,
        }
    }
}

impl Tool for ShellOutput {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &minicoding_core::tool::ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            let shell_id: String = params
                .get("shell_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("shell_id 缺失".into()))?
                .to_string();
            let status = store.output(shell_id).await?;
            // PTM-6（2026-08-25 R2 审查）：后台输出同样经脱敏——此前仅前台
            // shell.run 的 combined 输出走 redact，C-04 在 `shell.output` 路径
            // 整体旁路（后台命令输出凭证原样回灌 LLM/前端）。
            let stdout = minicoding_policy::redact(&status.stdout);
            let stderr = minicoding_policy::redact(&status.stderr);
            Ok(ToolResult::ok_json(serde_json::json!({
                "stdout": stdout,
                "stderr": stderr,
                "exited": status.exited,
                "exit_code": status.exit_code,
            })))
        })
    }

    /// 输出 JSON Schema（R-05，M-11）：stdout/stderr 快照 + 退出状态。
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
    use super::*;
    use crate::shell::{InMemoryBackgroundShellStore, ShellBackground};
    use minicoding_core::model::ToolContent;
    use minicoding_core::tool::ToolContext;

    fn make_store() -> Arc<dyn BackgroundShellStore> {
        Arc::new(InMemoryBackgroundShellStore::new())
    }

    fn make_ctx() -> ToolContext {
        ToolContext::new("/tmp".into(), "test".to_string())
    }

    #[tokio::test]
    async fn output_missing_shell_id_returns_invalid_input() {
        let tool = ShellOutput::new(make_store());
        let result = tool.execute(serde_json::json!({}), &make_ctx()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn output_nonexistent_returns_not_found() {
        let tool = ShellOutput::new(make_store());
        let result = tool
            .execute(serde_json::json!({"shell_id": "nonexistent"}), &make_ctx())
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn output_returns_json_with_status() {
        let store = make_store();
        // 先用 background 工具 spawn 一个命令
        let bg_tool = ShellBackground::new(Arc::clone(&store));
        let ctx = make_ctx();
        let bg_result = bg_tool
            .execute(serde_json::json!({"command": "echo test"}), &ctx)
            .await
            .expect("spawn");

        let ToolContent::Text(bg_text) = bg_result.content else {
            panic!("expected text");
        };
        // 提取 shell_id（格式："... shell_id=XXX ..."）
        let shell_id = bg_text
            .split("shell_id=")
            .nth(1)
            .and_then(|s| s.split(')').next())
            .expect("extract shell_id");

        // 等待命令完成。
        // 真实等待：spawn 的是真实子进程（echo test），完成时刻由 OS 调度
        // 决定，虚拟时钟无法加速（start_paused 不适用）
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // 读取输出
        let output_tool = ShellOutput::new(store);
        let result = output_tool
            .execute(serde_json::json!({"shell_id": shell_id}), &make_ctx())
            .await
            .expect("output");

        let ToolContent::Json(value) = result.content else {
            panic!("expected json content");
        };
        assert!(value["stdout"].as_str().unwrap().contains("test"));
        assert!(value["exited"].as_bool().unwrap());
    }

    #[test]
    fn output_side_effect_is_none() {
        let tool = ShellOutput::new(make_store());
        assert_eq!(tool.side_effect(), SideEffect::None);
    }

    #[test]
    fn output_schema_has_correct_name() {
        let tool = ShellOutput::new(make_store());
        assert_eq!(tool.name(), "shell.output");
    }

    #[test]
    fn output_declares_output_schema() {
        // R-05（M-11）：shell.output 声明输出 JSON 形态（stdout/stderr + 退出状态）。
        let tool = ShellOutput::new(make_store());
        let schema = tool.output_schema().expect("output schema");
        assert_eq!(schema.schema["type"], "object");
        assert_eq!(schema.schema["properties"]["stdout"]["type"], "string");
        assert!(
            schema.schema["properties"]
                .get("exit_code")
                .expect("exit_code")
                .is_object()
        );
        assert!(
            schema.schema["required"]
                .as_array()
                .expect("required")
                .iter()
                .any(|v| v == "exited")
        );
    }

    #[test]
    fn output_render_output_defaults_to_json() {
        let tool = ShellOutput::new(make_store());
        let result = ToolResult::ok_json(serde_json::json!({
            "stdout": "hi",
            "stderr": "",
            "exited": true,
            "exit_code": 0
        }));
        match tool.render_output(&result) {
            RenderIntent::Json { .. } => {}
            other => panic!("expected Json, got {other:?}"),
        }
    }
}
