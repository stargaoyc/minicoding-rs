//! `fs.list`：列目录内容。

use crate::util::{resolve_path, truncate_output};
use camino::Utf8PathBuf;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::{Value, json};

/// 列目录内容的只读工具。
pub struct FsList {
    schema: ToolSchema,
}

impl FsList {
    /// 创建 `fs.list` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "fs.list".to_string(),
            description: "列出目录内容，返回 JSON 数组（name/type/size）。".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "目录路径（相对路径基于工作目录解析）。"
                    },
                    "recursive": {
                        "type": "boolean",
                        "description": "是否递归列出（尊重 .gitignore），默认 false。"
                    }
                },
                "required": ["path"]
            }),
        };
        Self { schema }
    }
}

impl Default for FsList {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct ListInput {
    path: String,
    recursive: Option<bool>,
}

impl Tool for FsList {
    fn name(&self) -> &'static str {
        "fs.list"
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
            let args: ListInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
            let path = resolve_path(&workdir, &args.path)?;

            let entries: Vec<Value> = if args.recursive.unwrap_or(false) {
                list_recursive(&path)?
            } else {
                list_flat(&path).await?
            };

            let out = serde_json::to_string_pretty(&entries)
                .map_err(|e| ToolError::Exec(e.to_string()))?;
            let (text, truncated) = truncate_output(out, max_output_bytes);
            let bytes = text.len();
            let mut result = ToolResult::ok_text(text);
            result.metadata.truncated = truncated;
            result.metadata.bytes = bytes;
            Ok(result)
        })
    }
}

async fn list_flat(path: &Utf8PathBuf) -> Result<Vec<Value>, ToolError> {
    let mut entries = Vec::new();
    let mut reader = tokio::fs::read_dir(path)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ToolError::NotFound(path.to_string()),
            _ => ToolError::Io(e),
        })?;
    while let Some(entry) = reader.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = entry.metadata().await?;
        let entry_type = entry_type_of(&metadata);
        entries.push(json!({
            "name": name,
            "type": entry_type,
            "size": metadata.len(),
        }));
    }
    entries.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    Ok(entries)
}

fn list_recursive(path: &Utf8PathBuf) -> Result<Vec<Value>, ToolError> {
    let mut entries = Vec::new();
    let walker = ignore::WalkBuilder::new(path).build();
    for entry in walker {
        let entry = entry.map_err(|e| ToolError::Io(std::io::Error::other(e.to_string())))?;
        // 跳过根目录本身
        if entry.depth() == 0 {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|e| ToolError::Io(std::io::Error::other(e.to_string())))?;
        let entry_type = entry_type_of(&metadata);
        let rel = match entry.path().strip_prefix(path.as_std_path()) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => entry.path().to_string_lossy().to_string(),
        };
        entries.push(json!({
            "name": rel,
            "type": entry_type,
            "size": metadata.len(),
        }));
    }
    Ok(entries)
}

fn entry_type_of(metadata: &std::fs::Metadata) -> &'static str {
    if metadata.is_dir() {
        "dir"
    } else if metadata.is_file() {
        "file"
    } else {
        "symlink"
    }
}
