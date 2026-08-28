//! L1 工具结果裁剪（见 `docs/design.md` §3.3）。
//!
//! 对超过字符阈值的 `ContentBlock::ToolResult` 文本内容截断为
//! "前 K 行 + ... + 后 K 行 + 元信息"，保留边界标注（C-05：工具结果是数据非指令）。
//!
//! CTX-R6-11（2026-08-28 R8 审查）：裁剪改为**最大优先 + 预算内即停**——此前
//! 无条件裁掉所有超阈值工具结果，压缩超阈主因是历史消息累积时（大工具结果只占
//! 少量）也会把最近的大工具结果整体损毁。现按字符数降序逐个裁剪，每裁一个检查
//! 是否已回落到 token 预算内（是则停止），保留"够用即止"的最小破坏语义。

use minicoding_core::model::{ContentBlock, Message, ToolContent};
use minicoding_core::provider::Tokenizer;

use super::CompressResult;

/// L1 裁剪配置。
#[derive(Debug, Clone)]
pub struct ClipConfig {
    /// 字符阈值，超过则裁剪（默认 2000）。
    pub threshold_chars: usize,
    /// 保留首尾行数（默认 20）。
    pub keep_lines: usize,
}

impl Default for ClipConfig {
    fn default() -> Self {
        Self {
            threshold_chars: 2000,
            keep_lines: 20,
        }
    }
}

/// L1 工具结果裁剪（CTX-R6-11：最大优先 + 预算内即停）。
///
/// 收集全部超过 `threshold_chars` 的 ToolResult，按字符数降序逐个裁剪；每裁
/// 一个后若 `tokenizer.count_messages(messages) <= budget_threshold` 即停止。
/// 裁剪块数记入 `result.clipped_count`。
pub fn clip_tool_results(
    messages: &mut [Message],
    config: &ClipConfig,
    tokenizer: &dyn Tokenizer,
    budget_threshold: usize,
    result: &mut CompressResult,
) {
    // 收集超阈值 ToolResult 的（消息下标, 块下标, 字符数），按字符数降序。
    let mut oversized: Vec<(usize, usize, usize)> = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        for (bi, block) in msg.content.iter().enumerate() {
            if let ContentBlock::ToolResult { content, .. } = block
                && let Some(chars) = tool_content_chars(content)
                && chars > config.threshold_chars
            {
                oversized.push((mi, bi, chars));
            }
        }
    }
    oversized.sort_by_key(|&(_, _, chars)| std::cmp::Reverse(chars));

    for (mi, bi, _) in oversized {
        if tokenizer.count_messages(messages) <= budget_threshold {
            break;
        }
        if let ContentBlock::ToolResult { content, .. } = &mut messages[mi].content[bi]
            && clip_tool_content(content, config)
        {
            result.clipped_count += 1;
        }
    }
}

/// 计算 `ToolContent` 的可裁剪字符数（`Image` 返回 `None` 表示不可裁剪）。
fn tool_content_chars(content: &ToolContent) -> Option<usize> {
    match content {
        ToolContent::Text(text) => Some(text.chars().count()),
        ToolContent::Json(value) => serde_json::to_string_pretty(value)
            .ok()
            .map(|s| s.chars().count()),
        ToolContent::Mixed(parts) => {
            let total: usize = parts.iter().filter_map(tool_content_chars).sum();
            Some(total)
        }
        ToolContent::Image { .. } => None,
    }
}

/// 裁剪单个 `ToolContent`，返回是否实际裁剪。
fn clip_tool_content(content: &mut ToolContent, config: &ClipConfig) -> bool {
    match content {
        ToolContent::Text(text) => {
            if text.chars().count() <= config.threshold_chars {
                return false;
            }
            let before = text.chars().count();
            *text = clip_text(text, config.keep_lines);
            text.chars().count() < before
        }
        ToolContent::Json(value) => {
            // CTX-13（2026-08-26 R3 审查）：Json 工具结果此前完全不裁——一个
            // 500KB JSON 会直接把会话推进 L3/L4 丢历史，而它本可像 Text 一样
            // 首尾裁剪。pretty-print 后按行裁剪（保留结构首尾，中间省略）。
            // CT4-9（R4）：消除双重序列化——此前两次 `to_string_pretty` 调用，
            // 复用一次结果。
            let Ok(pretty) = serde_json::to_string_pretty(value) else {
                return false;
            };
            if pretty.chars().count() <= config.threshold_chars {
                return false;
            }
            let before = pretty.chars().count();
            let clipped = clip_text(&pretty, config.keep_lines);
            // 仅在确实缩短时替换
            if clipped.chars().count() < before {
                *value = serde_json::Value::String(clipped);
                true
            } else {
                false
            }
        }
        ToolContent::Mixed(parts) => {
            let mut clipped = false;
            for part in parts.iter_mut() {
                if clip_tool_content(part, config) {
                    clipped = true;
                }
            }
            clipped
        }
        ToolContent::Image { .. } => false,
    }
}

/// 按行裁剪文本：前 K 行 + ... + 后 K 行 + 元信息。
///
/// 行数足够时按行截断；行数不足但字符超阈值时按字符截断（每 "等效行" 约 80 字符）。
/// 保留 `[... omitted ...]` 边界标注（C-05：工具结果包裹边界）。
fn clip_text(text: &str, keep: usize) -> String {
    let total_chars = text.chars().count();
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    if total_lines > keep * 2 {
        // 行数足够：保留前 K 行 + 后 K 行
        let head = &lines[..keep];
        let tail = &lines[total_lines - keep..];
        let omitted = total_lines - keep * 2;
        format!(
            "{}\n... [{} lines omitted, {} chars total]\n{}",
            head.join("\n"),
            omitted,
            total_chars,
            tail.join("\n")
        )
    } else {
        // 行数不足但字符超阈值：按字符截断
        let chars: Vec<char> = text.chars().collect();
        // keep 行等效 ≈ keep * 80 字符
        let keep_chars = keep.saturating_mul(80);
        if chars.len() <= keep_chars * 2 {
            return text.to_string();
        }
        let head: String = chars[..keep_chars].iter().collect();
        let tail: String = chars[chars.len() - keep_chars..].iter().collect();
        format!(
            "{}\n... [{} chars omitted, {} chars total]\n{}",
            head,
            chars.len() - keep_chars * 2,
            total_chars,
            tail
        )
    }
}

/// 按字符数计数的分词器（1 字符 = 1 token，L1 测试用）。
/// 与 `TiktokenTokenizer::count_messages` 同口径——用 `full_text()`（含 `ToolResult`
/// 内容），否则 Tool 消息计 0 token、预算判据恒成立导致 L1 永不裁剪。
#[cfg(test)]
struct CharTokenizer;

#[cfg(test)]
impl Tokenizer for CharTokenizer {
    fn count(&self, text: &str) -> usize {
        text.chars().count()
    }
    fn count_messages(&self, msgs: &[Message]) -> usize {
        msgs.iter().map(|m| m.full_text().chars().count()).sum()
    }
    fn id(&self) -> &'static str {
        "char-test-clip"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::model::{ContentBlock, Message, ToolCallId, ToolContent};

    fn make_tool_result_msg(text: &str) -> Message {
        Message {
            id: ulid::Ulid::new().to_string(),
            role: minicoding_core::model::Role::Tool,
            content: vec![ContentBlock::ToolResult {
                call_id: String::new() as ToolCallId,
                content: ToolContent::Text(text.to_string()),
                is_error: false,
                metadata: minicoding_core::model::ToolResultMeta::default(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: time::OffsetDateTime::now_utc(),
            metadata: minicoding_core::model::MessageMeta::default(),
        }
    }

    #[test]
    fn clips_large_tool_result() {
        let big = "line\n".repeat(100); // 500 chars, 100 lines
        let msg = make_tool_result_msg(&big);
        let mut msgs = vec![msg.clone()];
        let mut result = CompressResult::default();
        let config = ClipConfig {
            threshold_chars: 200,
            keep_lines: 5,
        };
        clip_tool_results(&mut msgs, &config, &CharTokenizer, 0, &mut result);
        assert_eq!(result.clipped_count, 1);
        let clipped = match &msgs[0].content[0] {
            ContentBlock::ToolResult { content, .. } => match content {
                ToolContent::Text(t) => t,
                _ => panic!("expected text"),
            },
            _ => panic!("expected tool result"),
        };
        assert!(clipped.contains("lines omitted"));
        assert!(clipped.len() < big.len());
    }

    #[test]
    fn does_not_clip_small_tool_result() {
        let mut msgs = vec![make_tool_result_msg("small output")];
        let mut result = CompressResult::default();
        clip_tool_results(
            &mut msgs,
            &ClipConfig::default(),
            &CharTokenizer,
            0,
            &mut result,
        );
        assert_eq!(result.clipped_count, 0);
    }

    #[test]
    fn clips_few_lines_many_chars() {
        // 单行长文本：行数不足但字符超阈值，按字符截断
        let big = "x".repeat(5000);
        let mut msgs = vec![make_tool_result_msg(&big)];
        let mut result = CompressResult::default();
        let config = ClipConfig {
            threshold_chars: 2000,
            keep_lines: 20,
        };
        clip_tool_results(&mut msgs, &config, &CharTokenizer, 0, &mut result);
        assert_eq!(result.clipped_count, 1);
    }

    #[test]
    fn stops_clipping_once_under_budget() {
        // CTX-R6-11：预算已回落即停止裁剪——不再无谓损毁其余大工具结果。
        let big = "line\n".repeat(100); // 100 行 / ~600 字符
        let mut msgs = vec![make_tool_result_msg(&big), make_tool_result_msg(&big)];
        let mut result = CompressResult::default();
        let config = ClipConfig {
            threshold_chars: 200,
            keep_lines: 5,
        };
        // 预算 800：裁 1 个（100 行→10 行，约 70 字符）即回落到 70+600=670 ≤ 800
        clip_tool_results(&mut msgs, &config, &CharTokenizer, 800, &mut result);
        assert_eq!(result.clipped_count, 1, "预算内即停应只裁 1 个");

        // 预算 0 → 全部需裁剪
        let mut result2 = CompressResult::default();
        let mut msgs2 = vec![make_tool_result_msg(&big), make_tool_result_msg(&big)];
        clip_tool_results(&mut msgs2, &config, &CharTokenizer, 0, &mut result2);
        assert_eq!(result2.clipped_count, 2, "预算极小应全裁");
    }
}
