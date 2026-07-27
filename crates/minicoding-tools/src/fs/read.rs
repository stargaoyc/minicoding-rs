//! `fs.read`：读取文件内容（支持行范围）。

use crate::util::{resolve_path, truncate_output};
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;

/// 读取文件内容的只读工具。
pub struct FsRead {
    schema: ToolSchema,
}

impl FsRead {
    /// 创建 `fs.read` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.read".to_string(),
            description:
                "读取文件内容，支持行范围（offset 为起始行索引 0-based，limit 为返回行数）。"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径（相对路径基于工作目录解析）。"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "起始行索引（0-based），默认 0。"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "返回的最大行数，默认全部。"
                    }
                },
                "required": ["path"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsRead {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct ReadInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

impl Tool for FsRead {
    fn name(&self) -> &'static str {
        "fs.read"
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
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let workdir = ctx.workdir.clone();
        let max_output_bytes = ctx.max_output_bytes;
        Box::pin(async move {
            let args: ReadInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let path = resolve_path(&workdir, &args.path)?;

            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => ToolError::NotFound(args.path.clone()),
                    _ => ToolError::Io(e),
                })?;

            let lines: Vec<&str> = content.lines().collect();
            let offset = args.offset.unwrap_or(0);
            let limit = args.limit.unwrap_or(lines.len());

            let out: String = lines
                .iter()
                .skip(offset)
                .take(limit)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");

            let (text, truncated) = truncate_output(out, max_output_bytes);
            let bytes = text.len();
            let mut result = ToolResult::ok_text(text);
            result.metadata.truncated = truncated;
            result.metadata.bytes = bytes;
            Ok(result)
        })
    }
}
