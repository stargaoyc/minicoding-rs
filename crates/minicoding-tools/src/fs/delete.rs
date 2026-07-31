//! `fs.delete`：删除文件。

use crate::util::resolve_path;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;

/// 删除文件的工具。
pub struct FsDelete {
    schema: ToolSchema,
}

impl FsDelete {
    /// 创建 `fs.delete` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.delete".to_string(),
            description: "删除文件（路径不可越界，相对路径基于工作目录解析；文件不存在则报错）。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径（相对路径基于工作目录解析）。"
                    }
                },
                "required": ["path"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsDelete {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct DeleteInput {
    path: String,
}

impl Tool for FsDelete {
    fn name(&self) -> &'static str {
        "fs.delete"
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::FileWrite
    }

    fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let workdir = ctx.workdir.clone();
        Box::pin(async move {
            let args: DeleteInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let path = resolve_path(&workdir, &args.path)?;

            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => ToolError::NotFound(args.path.clone()),
                    _ => ToolError::Io(e),
                })?;

            Ok(ToolResult::ok_text(format!("deleted {}", args.path)))
        })
    }
}
