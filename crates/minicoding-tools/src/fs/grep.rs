//! `fs.grep`：正则搜索文件内容。

use crate::util::{ensure_dir, resolve_path, truncate_output};
use camino::Utf8PathBuf;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;
use std::fmt::Write;

/// 正则搜索文件内容的只读工具。
pub struct FsGrep {
    schema: ToolSchema,
}

impl FsGrep {
    /// 创建 `fs.grep` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.grep".to_string(),
            description: "按正则搜索文件内容（尊重 .gitignore），返回 file:line:content 匹配行。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "正则表达式。"
                    },
                    "path": {
                        "type": "string",
                        "description": "搜索根目录（相对路径基于工作目录解析），默认工作目录。"
                    },
                    "include": {
                        "type": "string",
                        "description": "文件名 glob 过滤（如 \"*.rs\"），仅搜索匹配文件。"
                    }
                },
                "required": ["pattern"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsGrep {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
}

impl Tool for FsGrep {
    fn name(&self) -> &'static str {
        "fs.grep"
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
            let args: GrepInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            let re = regex::Regex::new(&args.pattern)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            let include_matcher = match args.include {
                Some(g) => Some(
                    globset::Glob::new(&g)
                        .map_err(|e| ToolError::InvalidInput(e.to_string()))?
                        .compile_matcher(),
                ),
                None => None,
            };

            let base: Utf8PathBuf = match &args.path {
                Some(p) => resolve_path(&workdir, p)?,
                None => workdir.clone(),
            };
            ensure_dir(&base).await?;

            let mut out = String::new();
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
                if let Some(m) = &include_matcher {
                    let basename = match entry.path().file_name() {
                        Some(n) => n.to_string_lossy().to_string(),
                        None => String::new(),
                    };
                    if !m.is_match(&basename) {
                        continue;
                    }
                }
                let Ok(content) = tokio::fs::read_to_string(entry.path()).await else {
                    continue;
                };
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        let _ = writeln!(out, "{rel}:{}:{}", i + 1, line);
                    }
                }
            }

            let (text, truncated) = truncate_output(out, max_output_bytes);
            let bytes = text.len();
            let mut result = ToolResult::ok_text(text);
            result.metadata.truncated = truncated;
            result.metadata.bytes = bytes;
            Ok(result)
        })
    }
}
