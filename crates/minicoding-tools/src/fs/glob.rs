//! `fs.glob`：glob 模式匹配文件。

use crate::util::{ensure_dir, resolve_path, truncate_output};
use camino::Utf8PathBuf;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;

/// glob 模式匹配文件的只读工具。
pub struct FsGlob {
    schema: ToolSchema,
}

impl FsGlob {
    /// 创建 `fs.glob` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.glob".to_string(),
            description: "按 glob 模式匹配文件（尊重 .gitignore），返回匹配路径列表。".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "glob 模式（如 \"**/*.rs\"、\"src/*.toml\"）。"
                    },
                    "path": {
                        "type": "string",
                        "description": "搜索根目录（相对路径基于工作目录解析），默认工作目录。"
                    }
                },
                "required": ["pattern"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsGlob {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    path: Option<String>,
}

impl Tool for FsGlob {
    fn name(&self) -> &'static str {
        "fs.glob"
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
            let args: GlobInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            let matcher = globset::Glob::new(&args.pattern)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?
                .compile_matcher();

            let base: Utf8PathBuf = match &args.path {
                Some(p) => resolve_path(&workdir, p)?,
                None => workdir.clone(),
            };
            ensure_dir(&base).await?;

            let mut matched_paths = Vec::new();
            let walker = ignore::WalkBuilder::new(&base).build();
            for entry in walker {
                let entry =
                    entry.map_err(|e| ToolError::Io(std::io::Error::other(e.to_string())))?;
                let Some(ft) = entry.file_type() else {
                    continue;
                };
                if !ft.is_file() {
                    continue;
                }
                let rel = match entry.path().strip_prefix(base.as_std_path()) {
                    Ok(p) => p.to_string_lossy().to_string(),
                    Err(_) => entry.path().to_string_lossy().to_string(),
                };
                if matcher.is_match(&rel) {
                    matched_paths.push(rel);
                }
            }

            let out = matched_paths.join("\n");
            let (text, truncated) = truncate_output(out, max_output_bytes);
            let bytes = text.len();
            let mut result = ToolResult::ok_text(text);
            result.metadata.truncated = truncated;
            result.metadata.bytes = bytes;
            Ok(result)
        })
    }
}
