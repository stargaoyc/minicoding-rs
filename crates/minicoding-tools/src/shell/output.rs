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
