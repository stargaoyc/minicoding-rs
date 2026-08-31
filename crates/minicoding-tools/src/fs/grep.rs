//! `fs.grep`：正则搜索文件内容。

use crate::util::{ensure_dir, resolve_path, truncate_output};
use camino::Utf8PathBuf;
use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{ListItem, ListKind, RenderIntent, Tool, ToolContext};
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
            description: "按正则搜索文件内容（尊重 .gitignore），返回 file:line:content 匹配行；支持上下文行与匹配数上限。"
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
                    },
                    "context": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "每个匹配行前后各附带的上下文行数（默认 0，类似 grep -C）。"
                    },
                    "head_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "最多返回的匹配行数（超出截断，防止大仓库输出爆炸；默认无限制，仍受全局字节上限约束）。"
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
    #[serde(default)]
    context: usize,
    head_limit: Option<usize>,
}

/// 截断输出并包装为成功结果（walker 循环内提前返回与正常收尾共用）。
fn finish(out: String, max_output_bytes: usize) -> ToolResult {
    let (text, truncated) = truncate_output(out, max_output_bytes);
    let bytes = text.len();
    let mut result = ToolResult::ok_text(text);
    result.metadata.truncated = truncated;
    result.metadata.bytes = bytes;
    result
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

    // context/head_limit 分支使函数体略超 100 行；拆分反而打断
    // "解析→遍历→格式化"的线性可读性。
    #[allow(clippy::too_many_lines)]
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

            let ctx_lines = args.context;
            let head_limit = args.head_limit;
            // 遍历 + 匹配整体移入阻塞线程池（T-5，2026-08-25 审查）：`ignore` 的
            // WalkBuilder 是同步遍历，大仓库下会长时间占用 executor 线程饿死其他
            // task；阻塞线程内文件读取退化为 `std::fs`（线程池阻塞无碍）。
            // 收集完输出后回传 async 侧统一截断。
            let out = tokio::task::spawn_blocking(move || -> Result<String, ToolError> {
                let mut out = String::new();
                let mut matches_emitted = 0usize;
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
                    let Ok(content) = std::fs::read_to_string(entry.path()) else {
                        continue;
                    };
                    // R10-12：敏感文件（.env/credentials/*.pem 等）的匹配输出同样
                    // 脱敏——此前仅 `fs.read` 脱敏，`fs.grep pattern=".*" .env` 可
                    // 完整输出所有密钥绕过 C-04 防线。
                    let path_utf8 = camino::Utf8PathBuf::from_path_buf(entry.path().to_path_buf())
                        .unwrap_or_else(|p| {
                            camino::Utf8PathBuf::from(p.to_string_lossy().as_ref())
                        });
                    let content = if crate::fs::is_sensitive_path(&path_utf8) {
                        minicoding_policy::redact(&content)
                    } else {
                        content
                    };

                    if ctx_lines == 0 {
                        for (i, line) in content.lines().enumerate() {
                            if re.is_match(line) {
                                match head_limit {
                                    Some(limit) if matches_emitted >= limit => {
                                        return Ok(out);
                                    }
                                    _ => {}
                                }
                                let _ = writeln!(out, "{rel}:{}:{}", i + 1, line);
                                matches_emitted += 1;
                            }
                        }
                    } else {
                        // 带上下文行（grep -C 语义）：收集命中行号 → 合并区间 → 输出
                        // 命中行 + 前后各 ctx 行，区间间以 `--` 分隔（同文件内）。
                        let lines: Vec<&str> = content.lines().collect();
                        let mut ranges: Vec<(usize, usize)> = Vec::new();
                        for (i, line) in lines.iter().enumerate() {
                            if re.is_match(line) {
                                let start = i.saturating_sub(ctx_lines);
                                let end = (i + ctx_lines).min(lines.len().saturating_sub(1));
                                if let Some(last) = ranges.last_mut()
                                    && start <= last.1.saturating_add(1)
                                {
                                    last.1 = last.1.max(end);
                                } else {
                                    ranges.push((start, end));
                                }
                            }
                        }
                        'outer: for (start, end) in ranges {
                            for (i, line) in
                                lines.iter().enumerate().skip(start).take(end - start + 1)
                            {
                                match head_limit {
                                    Some(limit) if matches_emitted >= limit => break 'outer,
                                    _ => {}
                                }
                                let marker = if re.is_match(line) { ':' } else { '-' };
                                let _ = writeln!(out, "{rel}:{}{marker}{}", i + 1, line);
                                if marker == ':' {
                                    matches_emitted += 1;
                                }
                            }
                            if end < lines.len().saturating_sub(1) {
                                let _ = writeln!(out, "--");
                            }
                        }
                    }
                }
                Ok(out)
            })
            .await
            .map_err(|e| {
                ToolError::Io(std::io::Error::other(format!("fs.grep 后台遍历失败: {e}")))
            })??;

            Ok(finish(out, max_output_bytes))
        })
    }

    /// 渲染意图（R-05，M-11）：匹配行（`path:line:content`）→ 通用列表。
    ///
    /// 每行一个 `ListItem`（label=整行）。空结果/解析无关直接返回默认。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        match &result.content {
            minicoding_core::model::ToolContent::Text(text) => {
                let items: Vec<ListItem> = text
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| ListItem {
                        label: l.to_string(),
                        hint: None,
                    })
                    .collect();
                if items.is_empty() {
                    return RenderIntent::default_for(result);
                }
                RenderIntent::List {
                    items,
                    kind: ListKind::Generic,
                }
            }
            _ => RenderIntent::default_for(result),
        }
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

    #[tokio::test]
    async fn grep_context_lines_included() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "l1\nl2\nHIT\nl4\nl5\n").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        let result = tool
            .execute(json!({"pattern": "HIT", "path": ".", "context": 1}), &ctx)
            .await
            .expect("grep ok");
        let text = text_of(&result);
        // 命中行用 ':'，上下文行用 '-'（grep -C 惯例）
        assert!(text.contains("a.txt:2-l2"));
        assert!(text.contains("a.txt:3:HIT"));
        assert!(text.contains("a.txt:4-l4"));
        assert!(!text.contains("a.txt:1-"));
        assert!(!text.contains("a.txt:5-"));
    }

    #[tokio::test]
    async fn grep_head_limit_truncates_matches() {
        let (tmp, workdir) = make_workdir();
        std::fs::write(tmp.path().join("a.txt"), "m1\nm2\nm3\n").expect("write");
        let ctx = ToolContext::new(workdir, "test".to_string());
        let tool = FsGrep::new();
        let result = tool
            .execute(
                json!({"pattern": "m[0-9]", "path": ".", "head_limit": 2}),
                &ctx,
            )
            .await
            .expect("grep ok");
        let text = text_of(&result);
        assert!(text.contains("a.txt:1:m1"));
        assert!(text.contains("a.txt:2:m2"));
        assert!(
            !text.contains("a.txt:3:m3"),
            "超出 head_limit 的匹配应被截断"
        );
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

    #[test]
    fn render_output_projects_match_lines_to_list() {
        // R-05（M-11）：fs.grep 匹配行（path:line:content）→ List{Generic}。
        let tool = FsGrep::new();
        let result = ToolResult::ok_text("a.rs:3:pub fn main()\nb.rs:1:use std::fmt");
        match tool.render_output(&result) {
            RenderIntent::List { items, kind } => {
                assert_eq!(kind, ListKind::Generic);
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].label, "a.rs:3:pub fn main()");
                assert_eq!(items[1].label, "b.rs:1:use std::fmt");
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn render_output_empty_text_falls_back_to_default() {
        // 空结果 → 默认文本直出（回归保底）。
        let tool = FsGrep::new();
        let result = ToolResult::ok_text(String::new());
        match tool.render_output(&result) {
            RenderIntent::Text { .. } => {}
            other => panic!("expected Text fallback, got {other:?}"),
        }
    }
}
