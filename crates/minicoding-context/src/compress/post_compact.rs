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
use std::path::{Path, PathBuf};

use minicoding_core::model::{Message, Role};
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
        // 运行时 assistant 消息的工具调用存于 `msg.tool_calls` 字段
        // （DeltaAccumulator::finalize 构造），content 里只有 Text——
        // 此前扫描 ContentBlock::ToolUse 恒为空（死代码，2026-08-23 审查 §8-P0）。
        for tool_call in &msg.tool_calls {
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
    result
}

/// 将 post-compact 上下文注入 system 段。
///
/// 读取 `file_paths` 中的文件内容（按 `max_tokens_per_file` 截断），
/// 拼接成 `<post_compact_context>` 块追加到 system 段末尾。
/// 文件读取失败静默跳过。
///
/// CT-5（2026-08-25 审查）：文件读取用 `tokio::fs`——本函数在
/// `build_chat_request` 的 async 路径上执行，`std::fs` 阻塞读会卡住 executor
/// worker 线程。
///
/// # 返回
/// 注入后的新 system 段。若无文件可注入，返回原 system 段不变。
pub async fn inject_post_compact(
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
        // CTX-7（2026-08-25 R2 审查）：注入路径必须落在 workdir 内——绝对路径
        // 此前直接读取，TOCTOU 窗口内被换成 symlink 可把任意文件内容回灌进
        // system 段。组件级包含判定 + `..` 拒绝（与 journal validate 同口径）。
        let joined = if Path::new(path_str).is_absolute() {
            Path::new(path_str).to_path_buf()
        } else {
            workdir.join(path_str)
        };
        if !path_within_workdir(&joined, workdir) {
            tracing::debug!(
                file = %path_str,
                "post-compact: 跳过 workdir 外的路径"
            );
            continue;
        }
        let full_path = joined;

        let content = match tokio::fs::read_to_string(&full_path).await {
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
/// 组件级 workdir 包含判定（CTX-7，2026-08-28 R5 收尾）：拒绝 `..` 段；
/// 且两侧先做词法规范化（消解 workdir 自身的 `..`——调用方传入非规范化
/// workdir 时裸组件前缀比较会失配）。与 `memory::loader` 的
/// `resolve_lexical` 修复同模式。
fn path_within_workdir(path: &Path, workdir: &Path) -> bool {
    for comp in path.components() {
        if matches!(comp, std::path::Component::ParentDir) {
            return false;
        }
    }
    // 词法消解 workdir 的 `.`/`..` 段（path 侧已保证无 `..`，`.` 无害）
    let norm_workdir = normalize_lexical_workdir(workdir);
    let p: Vec<_> = path.components().collect();
    let w: Vec<_> = norm_workdir.components().collect();
    p.len() >= w.len() && p[..w.len()] == w[..]
}

/// 词法规范化 workdir（消解 `.`/`..` 段，不触碰文件系统、不解 symlink）。
fn normalize_lexical_workdir(workdir: &Path) -> PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    let mut has_root = false;
    for comp in workdir.components() {
        match comp {
            // CurDir/Prefix 忽略（不改变路径结构；Prefix 仅 Windows 前缀）
            std::path::Component::CurDir | std::path::Component::Prefix(_) => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::Normal(s) => parts.push(s.to_os_string()),
            std::path::Component::RootDir => {
                parts.clear();
                has_root = true;
            }
        }
    }
    let mut out = PathBuf::new();
    if has_root {
        out.push("/");
    }
    for p in parts {
        out.push(p);
    }
    out
}

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
            content: Vec::new(),
            tool_calls: vec![ToolCall {
                id: ToolCallId::new(),
                name: tool_name.to_string(),
                input: serde_json::json!({"path": path}),
            }],
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
        assert!(files.is_empty(), "expected empty: files");
    }

    #[tokio::test]
    async fn inject_post_compact_returns_original_when_no_files() {
        let tokenizer = CharTokenizer;
        let config = PostCompactConfig::default();
        let result = inject_post_compact("system", &[], &config, &tokenizer, Path::new(".")).await;
        assert_eq!(result, "system");
    }

    #[tokio::test]
    async fn inject_post_compact_skips_unreadable_files() {
        let tokenizer = CharTokenizer;
        let config = PostCompactConfig::default();
        let result = inject_post_compact(
            "system",
            &["nonexistent_file_xyz.rs".to_string()],
            &config,
            &tokenizer,
            Path::new("."),
        )
        .await;
        assert_eq!(result, "system");
    }

    #[tokio::test]
    async fn inject_post_compact_injects_readable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "fn main() {}").expect("write");
        let rel = "test.rs".to_string();

        let tokenizer = CharTokenizer;
        let config = PostCompactConfig::default();
        let result = inject_post_compact("system", &[rel], &config, &tokenizer, dir.path()).await;
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
