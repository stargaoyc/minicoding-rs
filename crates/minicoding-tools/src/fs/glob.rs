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

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::model::{SideEffect, ToolContent, ToolError};
    use minicoding_core::tool::Tool;
    use tempfile::TempDir;

    /// 创建临时 workdir 并返回 `(TempDir, 规范化后的 workdir 路径)`。
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

    /// 把结果文本按行拆分为路径列表。
    fn lines_of(result: &ToolResult) -> Vec<String> {
        let text = text_of(result);
        if text.is_empty() {
            Vec::new()
        } else {
            text.lines().map(String::from).collect()
        }
    }

    #[tokio::test]
    async fn glob_star_rs_matches_top_level_rs_files() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.rs"), "").expect("write");
        std::fs::write(tmp.path().join("b.rs"), "").expect("write");
        std::fs::write(tmp.path().join("c.txt"), "").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        let result = tool
            .execute(json!({"pattern": "*.rs", "path": "."}), &ctx)
            .await
            .expect("glob ok");
        assert!(!result.is_error);
        let mut paths = lines_of(&result);
        paths.sort();
        // *.rs 不跨目录边界，只匹配顶层 a.rs, b.rs
        assert_eq!(paths, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }

    #[tokio::test]
    async fn glob_double_star_matches_recursively() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.rs"), "").expect("write");
        std::fs::create_dir(tmp.path().join("sub")).expect("mkdir");
        std::fs::write(tmp.path().join("sub").join("b.rs"), "").expect("write");
        std::fs::write(tmp.path().join("sub").join("c.txt"), "").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        let result = tool
            .execute(json!({"pattern": "**/*.rs", "path": "."}), &ctx)
            .await
            .expect("glob ok");
        let mut paths = lines_of(&result);
        paths.sort();
        // **/*.rs 递归匹配所有 .rs 文件
        assert!(paths.contains(&"a.rs".to_string()));
        assert!(paths.contains(&"sub/b.rs".to_string()));
        // .txt 不匹配
        assert!(!paths.iter().any(|p| p.contains("c.txt")));
    }

    #[tokio::test]
    async fn glob_no_match_returns_empty() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        let result = tool
            .execute(json!({"pattern": "*.rs", "path": "."}), &ctx)
            .await
            .expect("glob ok");
        assert!(!result.is_error);
        assert!(text_of(&result).is_empty());
    }

    #[tokio::test]
    async fn glob_nonexistent_path_returns_not_found() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        let err = tool
            .execute(json!({"pattern": "*.rs", "path": "no_such_dir"}), &ctx)
            .await
            .unwrap_err();
        // ensure_dir 对不存在目录返回 NotFound
        assert!(
            matches!(err, ToolError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn glob_invalid_pattern_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        // 未闭合的字符类是非法 glob
        let err = tool
            .execute(json!({"pattern": "[unclosed", "path": "."}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn glob_default_path_is_workdir() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.rs"), "").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        // 省略 path → 默认 workdir
        let result = tool
            .execute(json!({"pattern": "*.rs"}), &ctx)
            .await
            .expect("glob ok");
        assert_eq!(lines_of(&result), vec!["a.rs".to_string()]);
    }

    #[test]
    fn glob_side_effect_is_none() {
        let tool = FsGlob::new();
        assert_eq!(tool.side_effect(), SideEffect::None);
        assert!(tool.is_read_only());
    }

    #[test]
    fn glob_schema_name_correct() {
        let tool = FsGlob::new();
        assert_eq!(tool.name(), "fs.glob");
        assert_eq!(tool.schema().name, "fs.glob");
    }

    #[tokio::test]
    async fn glob_missing_pattern_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGlob::new();
        let err = tool.execute(json!({"path": "."}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
