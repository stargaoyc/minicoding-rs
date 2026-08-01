//! 多会话侧栏渲染（T-M7-2）。
//!
//! 显示当前会话（高亮）+ 最近会话列表（按 `last_message_at` 倒序，截断前 20 条）。
//! 每条显示会话 ID 前 8 字符 + 消息数 + 相对时间。
//!
//! 切换会话：选中条目后按 `Enter` 发送 `UiCommand::SwitchSession(id)`，由
//! `runtime_bridge` 处理（加载新会话历史 + 重置上下文管理器）。
//!
//! ## 切换语义
//!
//! 切换到历史会话等价于 `--resume <id>`：保留原会话 ID，新消息追加写入原
//! JSONL 文件。当前会话不自动 summarize（避免阻塞 UI），用户可手动 `/summary`。

use minicoding_core::storage::SessionMeta;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use time::OffsetDateTime;

/// 渲染会话侧栏。
///
/// `sessions` 为已按 `last_message_at` 倒序排列的会话列表；
/// `current_id` 为当前会话 ID（高亮）；
/// `state` 为列表选择状态（键盘导航用）。
pub fn render_sidebar(
    frame: &mut Frame,
    area: Rect,
    sessions: &[SessionMeta],
    current_id: &str,
    state: &mut ListState,
) {
    let items: Vec<ListItem> = sessions
        .iter()
        .map(|s| {
            let is_current = s.id.as_str() == current_id;
            let prefix = if is_current { "▶ " } else { "  " };
            let id_short = short_id(&s.id);
            let time_str = relative_time(s.last_message_at);
            let line = Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Yellow)),
                Span::styled(id_short, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                Span::styled(
                    format!("({} 条)", s.message_count),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(" "),
                Span::styled(time_str, Style::default().fg(Color::DarkGray)),
            ]);
            if is_current {
                ListItem::new(line).style(Style::default().bg(Color::DarkGray))
            } else {
                ListItem::new(line)
            }
        })
        .collect();

    let block = Block::default()
        .borders(Borders::RIGHT)
        .title(" 会话 (F2 切换显示 / ↑↓ 选择 / Enter 恢复) ");
    let list = List::new(items).block(block).highlight_symbol("▶ ");
    frame.render_stateful_widget(list, area, state);
}

/// 取会话 ID 前 8 字符（足够区分，避免侧栏过宽）。
fn short_id(id: &str) -> String {
    if id.len() <= 8 {
        id.to_string()
    } else {
        id[..8].to_string()
    }
}

/// 相对时间："刚刚" / "5m" / "2h" / "3d" / "2026-01-01"。
fn relative_time(t: OffsetDateTime) -> String {
    let now = OffsetDateTime::now_utc();
    let delta = now - t;
    let secs = delta.whole_seconds();
    if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 7 * 86_400 {
        format!("{}d", secs / 86_400)
    } else {
        // 超过一周显示日期
        format!("{:04}-{:02}-{:02}", t.year(), t.month() as u8, t.day())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn short_id_truncates() {
        assert_eq!(short_id("abcdefgh1234"), "abcdefgh");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn relative_time_recent() {
        let now = OffsetDateTime::now_utc();
        let t = now - time::Duration::seconds(30);
        assert_eq!(relative_time(t), "刚刚");
    }

    #[test]
    fn relative_time_minutes() {
        let now = OffsetDateTime::now_utc();
        let t = now - time::Duration::seconds(300);
        assert_eq!(relative_time(t), "5m");
    }

    #[test]
    fn relative_time_hours() {
        let now = OffsetDateTime::now_utc();
        let t = now - time::Duration::seconds(7200);
        assert_eq!(relative_time(t), "2h");
    }

    #[test]
    fn relative_time_days() {
        let now = OffsetDateTime::now_utc();
        let t = now - time::Duration::seconds(3 * 86_400);
        assert_eq!(relative_time(t), "3d");
    }
}
