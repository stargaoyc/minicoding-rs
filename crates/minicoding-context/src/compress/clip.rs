//! L1 工具结果裁剪（见 `docs/design.md` §3.3）。
//!
//! 对超过字符阈值的 `ContentBlock::ToolResult` 文本内容截断为
//! "前 K 行 + ... + 后 K 行 + 元信息"，保留边界标注（C-05：工具结果是数据非指令）。

use minicoding_core::model::{ContentBlock, Message, ToolContent};

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

/// L1 工具结果裁剪。
///
/// 遍历所有消息的 `ContentBlock::ToolResult`，对其中 `ToolContent::Text`
/// 超过 `config.threshold_chars` 的文本按 "前 K 行 + ... + 后 K 行 + 元信息"
/// 截断。裁剪块数记入 `result.clipped_count`。
pub fn clip_tool_results(
    messages: &mut [Message],
    config: &ClipConfig,
    result: &mut CompressResult,
) {
    for msg in messages.iter_mut() {
        for block in &mut msg.content {
            if let ContentBlock::ToolResult { content, .. } = block
                && clip_tool_content(content, config)
            {
                result.clipped_count += 1;
            }
        }
    }
}

/// 裁剪单个 `ToolContent`，返回是否实际裁剪。
fn clip_tool_content(content: &mut ToolContent, config: &ClipConfig) -> bool {
    match content {
        ToolContent::Text(text) => {
            if text.chars().count() <= config.threshold_chars {
                return false;
            }
            *text = clip_text(text, config.keep_lines);
            true
        }
        ToolContent::Json(value) => {
            // CTX-13（2026-08-26 R3 审查）：Json 工具结果此前完全不裁——一个
            // 500KB JSON 会直接把会话推进 L3/L4 丢历史，而它本可像 Text 一样
            // 首尾裁剪。pretty-print 后按行裁剪（保留结构首尾，中间省略）。
            if serde_json::to_string_pretty(value)
                .map(|pretty| pretty.chars().count())
                .unwrap_or_default()
                <= config.threshold_chars
            {
                return false;
            }
            let Ok(pretty) = serde_json::to_string_pretty(value) else {
                return false;
            };
            let clipped = clip_text(&pretty, config.keep_lines);
            *value = serde_json::Value::String(clipped);
            true
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
        clip_tool_results(&mut msgs, &config, &mut result);
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
        clip_tool_results(&mut msgs, &ClipConfig::default(), &mut result);
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
        clip_tool_results(&mut msgs, &config, &mut result);
        assert_eq!(result.clipped_count, 1);
    }
}
