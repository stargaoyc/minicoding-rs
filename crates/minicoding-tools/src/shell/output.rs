//! `shell.output`：非阻塞读取后台 shell 已累积的输出（T-M8-5）。
//!
//! 返回 stdout/stderr 快照 + 是否已退出 + 退出码。多次调用返回增量累积内容
//! （非增量 diff，调用方需自行 diff 上次快照）。

use super::background::BackgroundShellStore;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::Tool;
use std::sync::Arc;

/// `shell.output` 工具：非阻塞读取后台 shell 输出。
pub struct ShellOutput {
    schema: ToolSchema,
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
        Self { schema, store }
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
            Ok(ToolResult::ok_json(serde_json::json!({
                "stdout": status.stdout,
                "stderr": status.stderr,
                "exited": status.exited,
                "exit_code": status.exit_code,
            })))
        })
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

        // 等待命令完成
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
}
