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

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::model::{SideEffect, ToolContent, ToolError};
    use minicoding_core::tool::Tool;
    use tempfile::TempDir;

    /// 创建临时 workdir 并返回 `(TempDir, 规范化后的 workdir 路径)`。
    /// 保留 `TempDir` 句柄以防止临时目录在测试结束前被清理。
    fn make_workdir() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().expect("create tempdir");
        let canon = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().expect("canonicalize"))
            .expect("utf-8 path");
        (tmp, canon)
    }

    /// 从 `ToolResult` 提取文本内容。
    fn text_of(result: &ToolResult) -> &str {
        match &result.content {
            ToolContent::Text(t) => t,
            _ => panic!("expected text content"),
        }
    }

    /// 把结果文本解析为 JSON 数组。
    fn parse_entries(result: &ToolResult) -> Vec<Value> {
        serde_json::from_str(text_of(result)).expect("parse json array")
    }

    #[tokio::test]
    async fn list_empty_directory_returns_empty_array() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsList::new();
        let result = tool
            .execute(json!({"path": "."}), &ctx)
            .await
            .expect("list ok");
        assert!(!result.is_error);
        assert!(parse_entries(&result).is_empty());
    }

    #[tokio::test]
    async fn list_directory_with_files_returns_filenames() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "hello").expect("write");
        std::fs::write(tmp.path().join("b.txt"), "world").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsList::new();
        let result = tool
            .execute(json!({"path": "."}), &ctx)
            .await
            .expect("list ok");
        let entries = parse_entries(&result);
        // 排序后顺序固定：a.txt, b.txt
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "a.txt");
        assert_eq!(entries[1]["name"], "b.txt");
        // 类型与 size 字段
        assert_eq!(entries[0]["type"], "file");
        assert_eq!(entries[0]["size"], 5); // "hello" = 5 字节
    }

    #[tokio::test]
    async fn list_directory_with_subdir_returns_subdir_entry() {
        let (tmp, workdir) = make_workdir();
        std::fs::create_dir(tmp.path().join("sub")).expect("mkdir");
        std::fs::write(tmp.path().join("a.txt"), "x").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsList::new();
        let result = tool
            .execute(json!({"path": "."}), &ctx)
            .await
            .expect("list ok");
        let entries = parse_entries(&result);
        // 非递归：只列出直接子项 a.txt + sub
        assert_eq!(entries.len(), 2);
        let sub = entries
            .iter()
            .find(|e| e["name"] == "sub")
            .expect("sub entry");
        assert_eq!(sub["type"], "dir");
    }

    #[tokio::test]
    async fn list_recursive_includes_subdir_contents() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "x").expect("write");
        std::fs::create_dir(tmp.path().join("sub")).expect("mkdir");
        std::fs::write(tmp.path().join("sub").join("b.txt"), "y").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsList::new();
        let result = tool
            .execute(json!({"path": ".", "recursive": true}), &ctx)
            .await
            .expect("list ok");
        let entries = parse_entries(&result);
        // 递归：a.txt, sub, sub/b.txt
        assert_eq!(entries.len(), 3);
        let names: Vec<String> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"sub".to_string()));
        assert!(names.contains(&"sub/b.txt".to_string()));
    }

    #[tokio::test]
    async fn list_nonexistent_directory_returns_not_found() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsList::new();
        let err = tool
            .execute(json!({"path": "no_such_dir"}), &ctx)
            .await
            .unwrap_err();
        // resolve_path 对不存在父目录返回 NotFound，list_flat 对不存在目录返回 NotFound
        assert!(
            matches!(err, ToolError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn list_path_escaped_rejected() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsList::new();
        let err = tool
            .execute(json!({"path": "../escape"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscaped(_)));
    }

    #[test]
    fn list_side_effect_is_none() {
        let tool = FsList::new();
        assert_eq!(tool.side_effect(), SideEffect::None);
        assert!(tool.is_read_only());
    }

    #[test]
    fn list_schema_name_correct() {
        let tool = FsList::new();
        assert_eq!(tool.name(), "fs.list");
        assert_eq!(tool.schema().name, "fs.list");
    }

    #[tokio::test]
    async fn list_missing_path_field_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsList::new();
        let err = tool.execute(json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
