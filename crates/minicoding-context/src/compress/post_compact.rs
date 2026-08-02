//! C-09：Post-compact 上下文恢复（见 `docs/design.md` §3.10）。
//!
//! 压缩后从历史提取最近 read 过的文件路径，按预算截断重新注入 system 段，
//! 避免模型在压缩后丢失文件上下文而重新 `fs.read`（减少工具调用 + token 消耗）。
//!
//! ## 流程
//!
//! 1. `extract_read_files`：扫描消息历史中的 `fs.read` tool call，提取文件路径
//!    （按最近优先，去重，最多 `max_files` 个）
//! 2. `inject_post_compact`：对每个文件路径，从磁盘读取内容并按 `max_tokens_per_file`
//!    截断，拼接成 `<post_compact_context>` 块注入 system 段末尾
//!
//! 文件读取失败（不存在/无权限）静默跳过，不阻塞压缩流程。

use std::collections::HashSet;
use std::path::Path;

use minicoding_core::model::{ContentBlock, Message, Role};
use minicoding_core::provider::Tokenizer;

/// C-09 post-compact 恢复配置。
#[derive(Debug, Clone)]
pub struct PostCompactConfig {
    /// 重新注入的文件数量上限。
    pub max_files: usize,
    /// 重新注入的 token 总预算。
    pub token_budget: usize,
    /// 单文件最大 token 数。
    pub max_tokens_per_file: usize,
}

impl Default for PostCompactConfig {
    fn default() -> Self {
        Self {
            max_files: 5,
            token_budget: 50_000,
            max_tokens_per_file: 5_000,
        }
    }
}

/// 从消息历史中提取最近 `fs.read` 调用读取过的文件路径。
///
/// 按最近优先扫描（从尾部向头部），去重，最多返回 `max_files` 个路径。
/// 只提取 `fs.read` 工具调用的 `path` 参数。
#[must_use]
pub fn extract_read_files(messages: &[Message], max_files: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();

    // 从最近的消息向头部扫描，优先保留最近读取的文件
    for msg in messages.iter().rev() {
        if msg.role != Role::Assistant {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::ToolUse(tool_call) = block {
                if tool_call.name != "fs.read" {
                    continue;
                }
                if let Some(path) = tool_call.input.get("path").and_then(|v| v.as_str())
                    && seen.insert(path.to_string())
                {
                    result.push(path.to_string());
                    if result.len() >= max_files {
                        return result;
                    }
                }
            }
        }
    }
    result
}

/// 将 post-compact 上下文注入 system 段。
///
/// 读取 `file_paths` 中的文件内容（按 `max_tokens_per_file` 截断），
/// 拼接成 `<post_compact_context>` 块追加到 system 段末尾。
/// 文件读取失败静默跳过。
///
/// # 返回
/// 注入后的新 system 段。若无文件可注入，返回原 system 段不变。
pub fn inject_post_compact(
    system_prompt: &str,
    file_paths: &[String],
    config: &PostCompactConfig,
    tokenizer: &dyn Tokenizer,
    workdir: &Path,
) -> String {
    if file_paths.is_empty() {
        return system_prompt.to_string();
    }

    let mut sections = Vec::new();
    let mut total_tokens = 0usize;

    for path_str in file_paths {
        let full_path = if Path::new(path_str).is_absolute() {
            Path::new(path_str).to_path_buf()
        } else {
            workdir.join(path_str)
        };

        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    file = %full_path.display(),
                    error = %e,
                    "post-compact: 跳过不可读文件"
                );
                continue;
            }
        };

        // 按 token 截断
        let truncated = truncate_to_tokens(&content, config.max_tokens_per_file, tokenizer);
        let section_tokens = tokenizer.count(&truncated);

        if total_tokens + section_tokens > config.token_budget {
            tracing::debug!(
                file = %path_str,
                tokens = section_tokens,
                budget_remaining = config.token_budget.saturating_sub(total_tokens),
                "post-compact: token 预算用尽，停止注入"
            );
            break;
        }

        total_tokens += section_tokens;
        sections.push(format!("--- {path_str} ---\n{truncated}"));
    }

    if sections.is_empty() {
        return system_prompt.to_string();
    }

    format!(
        "{system_prompt}\n\n<post_compact_context>\n以下是你最近读取过的文件内容（压缩后恢复），无需重新 read：\n\n{}\n</post_compact_context>",
        sections.join("\n\n")
    )
}

/// 按 token 数截断文本（保留前 `max_tokens` 个 token 对应的字符）。
fn truncate_to_tokens(text: &str, max_tokens: usize, tokenizer: &dyn Tokenizer) -> String {
    if tokenizer.count(text) <= max_tokens {
        return text.to_string();
    }
    // 二分查找截断点：找到 token 数 <= max_tokens 的最大字符数
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect();
        if tokenizer.count(&candidate) <= max_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let truncated: String = chars[..lo].iter().collect();
    if lo < chars.len() {
        format!(
            "{truncated}\n... (truncated, {} tokens)",
            tokenizer.count(&truncated)
        )
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::{Message, ToolCall, ToolCallId};
    use minicoding_core::provider::Tokenizer;

    /// 按字符数计数的分词器。
    struct CharTokenizer;
    impl Tokenizer for CharTokenizer {
        fn count(&self, text: &str) -> usize {
            text.chars().count()
        }
        fn count_messages(&self, msgs: &[Message]) -> usize {
            msgs.iter().map(|m| m.text().chars().count()).sum()
        }
        fn id(&self) -> &'static str {
            "char-test"
        }
    }

    fn make_assistant_msg_with_tool_call(tool_name: &str, path: &str) -> Message {
        Message {
            id: ulid::Ulid::new().to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse(ToolCall {
                id: ToolCallId::new(),
                name: tool_name.to_string(),
                input: serde_json::json!({"path": path}),
            })],
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: time::OffsetDateTime::now_utc(),
            metadata: minicoding_core::model::MessageMeta::default(),
        }
    }

    #[test]
    fn extract_read_files_finds_fs_read_calls() {
        let msgs = vec![
            make_assistant_msg_with_tool_call("fs.read", "src/main.rs"),
            make_assistant_msg_with_tool_call("fs.write", "src/lib.rs"),
            make_assistant_msg_with_tool_call("fs.read", "src/lib.rs"),
        ];
        let files = extract_read_files(&msgs, 5);
        // 最近优先：src/lib.rs 在最后，应排第一
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], "src/lib.rs");
        assert_eq!(files[1], "src/main.rs");
    }

    #[test]
    fn extract_read_files_deduplicates() {
        let msgs = vec![
            make_assistant_msg_with_tool_call("fs.read", "src/a.rs"),
            make_assistant_msg_with_tool_call("fs.read", "src/a.rs"),
            make_assistant_msg_with_tool_call("fs.read", "src/a.rs"),
        ];
        let files = extract_read_files(&msgs, 5);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "src/a.rs");
    }

    #[test]
    fn extract_read_files_respects_max_files() {
        let msgs = vec![
            make_assistant_msg_with_tool_call("fs.read", "a.rs"),
            make_assistant_msg_with_tool_call("fs.read", "b.rs"),
            make_assistant_msg_with_tool_call("fs.read", "c.rs"),
        ];
        let files = extract_read_files(&msgs, 2);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn extract_read_files_ignores_non_fs_read() {
        let msgs = vec![
            make_assistant_msg_with_tool_call("shell.run", "/etc/passwd"),
            make_assistant_msg_with_tool_call("fs.list", "src/"),
        ];
        let files = extract_read_files(&msgs, 5);
        assert!(files.is_empty());
    }

    #[test]
    fn inject_post_compact_returns_original_when_no_files() {
        let tokenizer = CharTokenizer;
        let config = PostCompactConfig::default();
        let result = inject_post_compact("system", &[], &config, &tokenizer, Path::new("."));
        assert_eq!(result, "system");
    }

    #[test]
    fn inject_post_compact_skips_unreadable_files() {
        let tokenizer = CharTokenizer;
        let config = PostCompactConfig::default();
        let result = inject_post_compact(
            "system",
            &["nonexistent_file_xyz.rs".to_string()],
            &config,
            &tokenizer,
            Path::new("."),
        );
        assert_eq!(result, "system");
    }

    #[test]
    fn inject_post_compact_injects_readable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "fn main() {}").expect("write");
        let rel = "test.rs".to_string();

        let tokenizer = CharTokenizer;
        let config = PostCompactConfig::default();
        let result = inject_post_compact("system", &[rel], &config, &tokenizer, dir.path());
        assert!(result.contains("<post_compact_context>"));
        assert!(result.contains("fn main() {}"));
        assert!(result.contains("test.rs"));
    }

    #[test]
    fn truncate_to_tokens_preserves_short_text() {
        let tokenizer = CharTokenizer;
        let result = truncate_to_tokens("hello", 100, &tokenizer);
        assert_eq!(result, "hello");
    }

    #[test]
    fn truncate_to_tokens_cuts_long_text() {
        let tokenizer = CharTokenizer;
        let result = truncate_to_tokens("abcdefghij", 5, &tokenizer);
        assert!(result.contains("abcde"));
        assert!(result.contains("truncated"));
    }
}
