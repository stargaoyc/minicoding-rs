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

    #[tokio::test]
    async fn grep_matches_pattern_returns_lines() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "foo\nbar\nfoobar\n").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        let result = tool
            .execute(json!({"pattern": "foo", "path": "."}), &ctx)
            .await
            .expect("grep ok");
        assert!(!result.is_error);
        let text = text_of(&result);
        // 匹配 foo 的行：第 1 行 "foo"、第 3 行 "foobar"
        assert!(text.contains("a.txt:1:foo"));
        assert!(text.contains("a.txt:3:foobar"));
        // "bar" 行不应被字面 "foo" 匹配
        assert!(!text.contains("a.txt:2:"));
    }

    #[tokio::test]
    async fn grep_no_match_returns_empty() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "hello\nworld\n").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        let result = tool
            .execute(json!({"pattern": "nonexistent_pattern", "path": "."}), &ctx)
            .await
            .expect("grep ok");
        assert!(!result.is_error);
        assert!(text_of(&result).is_empty());
    }

    #[tokio::test]
    async fn grep_regex_match_returns_matches() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "abc123\ndef456\nxyz\n").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        // 正则：字母后跟数字
        let result = tool
            .execute(json!({"pattern": "[a-z]+[0-9]+", "path": "."}), &ctx)
            .await
            .expect("grep ok");
        let text = text_of(&result);
        assert!(text.contains("a.txt:1:abc123"));
        assert!(text.contains("a.txt:2:def456"));
        // 第 3 行 "xyz" 不匹配
        assert!(!text.contains("a.txt:3:"));
    }

    #[tokio::test]
    async fn grep_multiple_files_returns_all_matches() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "target\n").expect("write");
        std::fs::write(tmp.path().join("b.txt"), "target line\n").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        let result = tool
            .execute(json!({"pattern": "target", "path": "."}), &ctx)
            .await
            .expect("grep ok");
        let text = text_of(&result);
        assert!(text.contains("a.txt:1:target"));
        assert!(text.contains("b.txt:1:target line"));
    }

    #[tokio::test]
    async fn grep_nonexistent_path_returns_not_found() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        let err = tool
            .execute(json!({"pattern": "foo", "path": "no_such_dir"}), &ctx)
            .await
            .unwrap_err();
        // ensure_dir 对不存在目录返回 NotFound
        assert!(
            matches!(err, ToolError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn grep_invalid_regex_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        // 未闭合的字符类
        let err = tool
            .execute(json!({"pattern": "[unclosed", "path": "."}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn grep_include_filter_restricts_files() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.rs"), "match\n").expect("write");
        std::fs::write(tmp.path().join("b.txt"), "match\n").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        let result = tool
            .execute(
                json!({"pattern": "match", "path": ".", "include": "*.rs"}),
                &ctx,
            )
            .await
            .expect("grep ok");
        let text = text_of(&result);
        // 只搜索 *.rs 文件
        assert!(text.contains("a.rs:1:match"));
        assert!(!text.contains("b.txt"));
    }

    #[tokio::test]
    async fn grep_default_path_is_workdir() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "findme\n").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        // 省略 path → 默认 workdir
        let result = tool
            .execute(json!({"pattern": "findme"}), &ctx)
            .await
            .expect("grep ok");
        assert!(text_of(&result).contains("a.txt:1:findme"));
    }

    #[test]
    fn grep_side_effect_is_none() {
        let tool = FsGrep::new();
        assert_eq!(tool.side_effect(), SideEffect::None);
        assert!(tool.is_read_only());
    }

    #[test]
    fn grep_schema_name_correct() {
        let tool = FsGrep::new();
        assert_eq!(tool.name(), "fs.grep");
        assert_eq!(tool.schema().name, "fs.grep");
    }

    #[tokio::test]
    async fn grep_missing_pattern_returns_invalid_input() {
        let (_tmp, workdir) = make_workdir();
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        let err = tool.execute(json!({"path": "."}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
