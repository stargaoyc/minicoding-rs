//! 流式 Markdown 增量渲染（T-M7-2）。
//!
//! 把 Markdown 文本转为 `ratatui::text::Line` 列表，支持标题/粗体/斜体/行内代码/
//! 代码块/列表/引用。流式场景下每帧重新解析（典型聊天长度 < 10KB，解析成本可忽略），
//! ratatui 双缓冲保证无闪烁。
//!
//! ## 为何不用 `pulldown-cmark` 等完整解析器
//!
//! ratatui 的 `Line` 是行级渲染单元，inline span 需手动构建。完整 Markdown AST
//! 转换到 `Line` 的胶水代码量与手写行级解析相当，且引入重依赖（`pulldown-cmark`
//! ~50KB）。聊天场景的 Markdown 子集有限，行级解析足够（见 `design.md` §25 权衡）。
//!
//! ## 增量策略
//!
//! 流式 token 拼接到 `streaming: String`，每帧调用 `parse_markdown(&streaming)`
//! 重新生成 `Vec<Line>`。ratatui 的 `Terminal::draw` 双缓冲整屏替换，旧帧与新帧
//! 原子切换，视觉上无闪烁（C-18：增量渲染只刷新脏区——ratatui 内部 diff 计算
//! 脏区，本层只保证解析结果稳定）。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

/// 解析 Markdown 文本为 `Line` 列表。
///
/// 行级解析：每行独立判断块类型（标题/列表/引用/代码块/段落），inline span
/// （`**bold**`/`*italic*`/`` `code` ``）由 `parse_inline` 处理。
#[must_use]
pub fn parse_markdown(text: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut code_block_lang: Option<String> = None;
    let mut code_block_lines: Vec<String> = Vec::new();

    for raw in text.lines() {
        // 代码块开关：```lang / ```
        if raw.trim_start().starts_with("```") {
            if in_code_block {
                // 关闭代码块：刷出已累积的行
                lines.push(render_code_block(
                    &code_block_lines,
                    code_block_lang.as_deref(),
                ));
                code_block_lines.clear();
                code_block_lang = None;
                in_code_block = false;
            } else {
                // 开启代码块：记录语言（用于未来语法高亮，当前仅显示）
                let lang = raw.trim_start().trim_start_matches("```").trim();
                if !lang.is_empty() {
                    code_block_lang = Some(lang.to_string());
                }
                in_code_block = true;
            }
            continue;
        }
        if in_code_block {
            code_block_lines.push(raw.to_string());
            continue;
        }
        // 空行
        if raw.trim().is_empty() {
            lines.push(Line::raw(""));
            continue;
        }
        // 标题 # / ## / ###
        if let Some(stripped) = raw.strip_prefix("# ") {
            lines.push(Line::from(vec![Span::styled(
                stripped.to_string(),
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Blue),
            )]));
            continue;
        }
        if let Some(stripped) = raw.strip_prefix("## ") {
            lines.push(Line::from(vec![Span::styled(
                stripped.to_string(),
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Cyan),
            )]));
            continue;
        }
        if let Some(stripped) = raw.strip_prefix("### ") {
            lines.push(Line::from(vec![Span::styled(
                stripped.to_string(),
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Magenta),
            )]));
            continue;
        }
        // 引用 >
        if let Some(stripped) = raw.strip_prefix("> ") {
            lines.push(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(Color::DarkGray)),
                Span::styled(stripped.to_string(), Style::default().fg(Color::DarkGray)),
            ]));
            continue;
        }
        // 无序列表 - / *
        if let Some(stripped) = raw.strip_prefix("- ").or_else(|| raw.strip_prefix("* ")) {
            let mut spans = vec![Span::styled("  • ", Style::default().fg(Color::Yellow))];
            spans.extend(parse_inline(stripped));
            lines.push(Line::from(spans));
            continue;
        }
        // 有序列表 1. 2. ...
        if let Some((num, rest)) = split_ordered_list_item(raw) {
            let mut spans = vec![Span::styled(
                format!("  {num}. "),
                Style::default().fg(Color::Yellow),
            )];
            spans.extend(parse_inline(&rest));
            lines.push(Line::from(spans));
            continue;
        }
        // 水平分割线 ---
        if raw.trim() == "---" || raw.trim() == "***" {
            lines.push(Line::from(vec![Span::styled(
                "─".repeat(40),
                Style::default().fg(Color::DarkGray),
            )]));
            continue;
        }
        // 段落：inline 解析
        lines.push(Line::from(parse_inline(raw)));
    }

    // 流式中：代码块未闭合，刷出已累积的行（避免等待 ``` 才显示）
    if in_code_block && !code_block_lines.is_empty() {
        lines.push(render_code_block(
            &code_block_lines,
            code_block_lang.as_deref(),
        ));
    }

    lines
}

/// 解析 inline span：`**bold**` / `*italic*` / `` `code` ``。
///
/// 简单状态机：扫描标记对，匹配成功生成 styled span，否则原样输出。
/// 不支持嵌套（如 `**bold *italic*`）——聊天场景罕见，复杂度收益不划算。
fn parse_inline(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // **bold**
        if bytes[i] == b'*'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'*'
            && let Some(end) = find_marker(text, i + 2, "**")
        {
            if !buf.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut buf)));
            }
            spans.push(Span::styled(
                text[i + 2..end].to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            i = end + 2;
            continue;
        }
        // *italic*
        if bytes[i] == b'*'
            && let Some(end) = find_marker(text, i + 1, "*")
        {
            if !buf.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut buf)));
            }
            spans.push(Span::styled(
                text[i + 1..end].to_string(),
                Style::default().add_modifier(Modifier::ITALIC),
            ));
            i = end + 1;
            continue;
        }
        // `code`
        if bytes[i] == b'`'
            && let Some(end) = find_marker(text, i + 1, "`")
        {
            if !buf.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut buf)));
            }
            spans.push(Span::styled(
                text[i + 1..end].to_string(),
                Style::default().fg(Color::Green).bg(Color::Black),
            ));
            i = end + 1;
            continue;
        }
        // 普通字符：按 UTF-8 字符边界解码追加到缓冲
        // FE-3（2026-08-27 R5 审查）：此前 `buf.push(bytes[i] as char)` 逐字节
        // 转 char——多字节 UTF-8（CJK/emoji）每个字节成一个乱码 char。marker
        // 判定是 ASCII 字节安全，此处取完整字符并按其字节宽度推进游标。
        match text[i..].chars().next() {
            Some(ch) => {
                buf.push(ch);
                i += ch.len_utf8();
            }
            None => i += 1, // 不可达（bytes 非空），防御性推进防死循环
        }
    }
    if !buf.is_empty() {
        spans.push(Span::raw(buf));
    }
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    spans
}

/// 从 `start` 开始查找 `marker`，返回其起始字节偏移（未找到返回 `None`）。
fn find_marker(text: &str, start: usize, marker: &str) -> Option<usize> {
    text[start..].find(marker).map(|p| start + p)
}

/// 拆分有序列表项：`1. text` → `("1", "text")`。
fn split_ordered_list_item(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim_start();
    let dot_pos = trimmed.find(". ")?;
    let num_part = &trimmed[..dot_pos];
    if num_part.chars().all(char::is_numeric) {
        let rest = &trimmed[dot_pos + 2..];
        Some((num_part.to_string(), rest.to_string()))
    } else {
        None
    }
}

/// 渲染代码块为单行 `Line`（多行用 `\n` 连接，由 `Paragraph::wrap` 折行显示）。
///
/// 代码块用边框 + 等宽色（`Color::Green`）标识。语言标识显示在块首（如 `rust`）。
fn render_code_block(lines: &[String], lang: Option<&str>) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some(l) = lang {
        spans.push(Span::styled(
            format!(" [{l}]\n"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled(
        lines.join("\n"),
        Style::default().fg(Color::Green),
    ));
    Line::from(spans)
}

/// 把 `Line` 列表包装为带边框的 `Paragraph`，供 `view/chat.rs` 直接渲染。
#[must_use]
pub fn markdown_paragraph<'a>(lines: Vec<Line<'a>>, title: &str, scroll: u16) -> Paragraph<'a> {
    Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::TOP)
                .title(format!(" {title} ")),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn parse_headers() {
        let lines = parse_markdown("# Title\n## Sub\n### Subsub");
        assert_eq!(lines.len(), 3);
        // 每行 1 个 span
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[1].spans.len(), 1);
        assert_eq!(lines[2].spans.len(), 1);
    }

    #[test]
    fn parse_code_block() {
        let md = "```rust\nfn main() {}\n```\n";
        let lines = parse_markdown(md);
        // 开启行 + 代码行（累积）+ 关闭行刷出 = 应为 1 个代码块行
        // 实际：```rust 行触发开启，`fn main() {}` 累积，``` 行触发关闭刷出 1 行
        assert_eq!(lines.len(), 1);
        // 代码块行含 [rust] 标识 + 代码内容
        assert!(lines[0].spans.iter().any(|s| s.content.contains("[rust]")));
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains("fn main() {}"))
        );
    }

    #[test]
    fn parse_streaming_unclosed_code_block() {
        // 流式：代码块未闭合，已累积的行应被刷出
        let md = "```python\nprint(1)\nprint(2)";
        let lines = parse_markdown(md);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains("print(1)"))
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains("print(2)"))
        );
    }

    #[test]
    fn parse_inline_bold() {
        let spans = parse_inline("hello **world** end");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "hello ");
        assert_eq!(spans[1].content, "world");
        assert_eq!(spans[2].content, " end");
    }

    #[test]
    fn parse_inline_italic() {
        let spans = parse_inline("a *b* c");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "a ");
        assert_eq!(spans[1].content, "b");
        assert_eq!(spans[2].content, " c");
    }

    #[test]
    fn parse_inline_code() {
        let spans = parse_inline("use `fmt` module");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content, "fmt");
    }

    #[test]
    fn parse_unordered_list() {
        let lines = parse_markdown("- item one\n- item two");
        assert_eq!(lines.len(), 2);
        // 每行：bullet span + content span
        assert!(lines[0].spans.iter().any(|s| s.content.contains("•")));
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains("item one"))
        );
    }

    #[test]
    fn parse_ordered_list() {
        let lines = parse_markdown("1. first\n2. second");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].spans.iter().any(|s| s.content.contains("1.")));
        assert!(lines[0].spans.iter().any(|s| s.content.contains("first")));
    }

    #[test]
    fn parse_blockquote() {
        let lines = parse_markdown("> quoted text");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.iter().any(|s| s.content.contains("▌")));
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains("quoted text"))
        );
    }

    #[test]
    fn parse_horizontal_rule() {
        let lines = parse_markdown("before\n---\nafter");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].spans.iter().any(|s| s.content.starts_with('─')));
    }

    #[test]
    fn parse_empty_input() {
        let lines = parse_markdown("");
        assert!(lines.is_empty(), "expected empty: lines");
    }

    #[test]
    fn split_ordered_list_item_valid() {
        let (num, rest) = split_ordered_list_item("1. hello").unwrap();
        assert_eq!(num, "1");
        assert_eq!(rest, "hello");
    }

    #[test]
    fn split_ordered_list_item_invalid() {
        assert!(split_ordered_list_item("not a list").is_none());
        assert!(split_ordered_list_item("a. not numeric").is_none());
    }
}
