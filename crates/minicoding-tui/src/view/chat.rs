//! 对话主视图渲染（T-M7-2）。
//!
//! 把聊天历史 + 流式 token 渲染为 `ratatui::text::Line` 列表。assistant 文本走
//! [`parse_markdown`] 增量解析，user/tool/system 用纯文本 + 角色前缀色。
//!
//! 自动滚到底部：计算 `scroll` 偏移使最后一行可见（与 T-M7-1 行为一致）。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::ChatLine;
use crate::render::markdown::parse_markdown;

/// 渲染对话主视图。
///
/// `lines` 为已固化的聊天历史，`streaming` 为未固化的 assistant token 流。
/// 各角色前缀使用固定颜色（user=cyan/assistant=green/tool=magenta/system=gray）。
pub fn render_chat(frame: &mut Frame, area: Rect, lines: &[ChatLine], streaming: &str) {
    let mut render_lines: Vec<Line> = Vec::new();
    for line in lines {
        match line {
            ChatLine::User(text) => {
                render_lines.push(Line::from(vec![
                    Span::styled("You: ", Style::default().fg(Color::Cyan)),
                    Span::raw(text.as_str()),
                ]));
            }
            ChatLine::Assistant(text) => {
                // assistant 文本走 Markdown 解析（T-M7-2）
                let md_lines = parse_markdown(text);
                if md_lines.is_empty() {
                    render_lines.push(Line::from(vec![
                        Span::styled("Assistant: ", Style::default().fg(Color::Green)),
                        Span::raw(""),
                    ]));
                } else {
                    // 首行加 "Assistant: " 前缀，其余行无前缀
                    let mut iter = md_lines.into_iter();
                    if let Some(first) = iter.next() {
                        let mut spans = vec![Span::styled(
                            "Assistant: ",
                            Style::default().fg(Color::Green),
                        )];
                        spans.extend(first.spans);
                        render_lines.push(Line::from(spans));
                    }
                    for l in iter {
                        render_lines.push(l);
                    }
                }
            }
            ChatLine::Tool { tool, done } => {
                let mark = if *done { "✓" } else { "…" };
                render_lines.push(Line::from(vec![
                    Span::styled(
                        format!("Tool [{mark}] "),
                        Style::default().fg(Color::Magenta),
                    ),
                    Span::raw(tool.as_str()),
                ]));
            }
            ChatLine::System(text) => {
                render_lines.push(Line::from(vec![
                    Span::styled("System: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(text.as_str(), Style::default().add_modifier(Modifier::DIM)),
                ]));
            }
        }
    }
    // 流式 token（未固化为消息）：同样走 Markdown 解析
    if !streaming.is_empty() {
        let md_lines = parse_markdown(streaming);
        let mut iter = md_lines.into_iter();
        if let Some(first) = iter.next() {
            let mut spans = vec![Span::styled(
                "Assistant: ",
                Style::default().fg(Color::Green),
            )];
            spans.extend(first.spans);
            render_lines.push(Line::from(spans));
        }
        for l in iter {
            render_lines.push(l);
        }
    }

    // 自动滚到底部：计算 scroll 偏移使最后一行可见
    let visible_height = usize::from(area.height.saturating_sub(2)); // 减边框
    let total = render_lines.len();
    let scroll = u16::try_from(total.saturating_sub(visible_height)).unwrap_or(u16::MAX);

    let block = Block::default().borders(Borders::TOP).title(" 对话 ");
    let para = Paragraph::new(render_lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(para, area);
}
