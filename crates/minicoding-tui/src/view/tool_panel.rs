//! 工具调用进度面板 + 任务面板（T-M7-4）。
//!
//! 显示当前 turn 的工具调用列表（进行中/已完成）+ 任务面板占位（订阅
//! `Event::TaskUpdated` 同步刷新，未来扩展）。
//!
//! 工具面板从 `App::lines` 中提取 `ChatLine::Tool` 行，按时间倒序显示最近 N 条。
//! 任务面板当前为占位（M7 骨架，`task.list`/`task.update` 事件订阅在 M7+ 接入）。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::ChatLine;

/// 渲染工具调用进度面板（T-M7-4）。
///
/// 从 `lines` 末尾提取 `ChatLine::Tool` 行，显示最近 `max_items` 条。
/// 进行中（`done=false`）用黄色 `…`，已完成用绿色 `✓`。
pub fn render_tool_panel(frame: &mut Frame, area: Rect, lines: &[ChatLine]) {
    // 从末尾收集工具行，倒序显示最近 N 条
    let max_items = usize::from(area.height.saturating_sub(2)); // 减边框
    let tool_lines: Vec<&ChatLine> = lines
        .iter()
        .rev()
        .filter(|l| matches!(l, ChatLine::Tool { .. }))
        .take(max_items)
        .collect();

    let mut render_lines: Vec<Line> = Vec::new();
    for tool in tool_lines.iter().rev() {
        if let ChatLine::Tool { tool: name, done } = tool {
            let (mark, color) = if *done {
                ("✓", Color::Green)
            } else {
                ("…", Color::Yellow)
            };
            render_lines.push(Line::from(vec![
                Span::styled(format!("[{mark}] "), Style::default().fg(color)),
                Span::raw(name.as_str()),
            ]));
        }
    }
    if render_lines.is_empty() {
        render_lines.push(Line::from(vec![Span::styled(
            "（暂无工具调用）",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 工具调用 (F3 切换) ");
    let para = Paragraph::new(render_lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}
