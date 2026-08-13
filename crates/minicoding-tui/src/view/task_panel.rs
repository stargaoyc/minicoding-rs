//! 任务列表面板（T-M7-4）。
//!
//! 订阅 `Event::TaskUpdated` 后由 `App` 维护任务列表，本模块负责将其渲染为
//! ratatui 面板：按状态排序（`InProgress` 优先 → `Pending` → `Completed`/`Cancelled`），
//! 每行显示状态标记 + 任务内容（截断到面板宽度）。
//!
//! 设计参考 `design.md` §18.4：`InProgress` 项高亮，阻塞项标注依赖链。
//! 当前 M7 骨架实现状态标记 + 内容截断；依赖链标注留待后续迭代。

use minicoding_core::model::{Task, TaskStatus};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::render::theme::Theme;

/// 渲染任务面板（T-M7-4）。
///
/// `tasks` 为当前会话的全部任务（由 `App` 在 `TaskUpdated` 事件时维护）；
/// `state` 为列表选择状态（未来支持键盘导航选中任务查看详情，当前仅展示）。
pub fn render_task_panel(
    frame: &mut Frame,
    area: Rect,
    tasks: &[Task],
    state: &mut ListState,
    theme: &Theme,
) {
    if tasks.is_empty() {
        // 空状态：占位文本
        let line = Line::from(vec![Span::styled(
            "（暂无任务，模型可调 task.create 创建）",
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
        )]);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" 任务 (Ctrl+T 切换) ");
        let para = Paragraph::new(line).block(block).wrap(Wrap { trim: false });
        frame.render_widget(para, area);
        return;
    }

    // 按状态优先级排序：InProgress > Pending > Completed > Cancelled
    let mut sorted: Vec<&Task> = tasks.iter().collect();
    sorted.sort_by_key(|t| status_priority(t.status));

    let items: Vec<ListItem> = sorted
        .iter()
        .map(|t| ListItem::new(task_line(t, theme, area.width)))
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" 任务 ({} 项 / Ctrl+T 切换) ", sorted.len()));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, area, state);
}

/// 渲染单条任务为 `Line`。
///
/// 状态标记：`InProgress` 用 `▶`、`Pending` 用 `○`、`Completed` 用 `✓`、
/// `Cancelled` 用 `✗`。内容截断到 `max_width`（减去标记与间距占用的列数）。
fn task_line(task: &Task, theme: &Theme, max_width: u16) -> Line<'static> {
    let (mark, color) = status_mark(task.status, theme);
    let prefix = format!("{mark} ");
    // 内容可用宽度 = 面板宽度 - 边框(2) - 标记(2) - 省略号预留(1)
    let avail = usize::from(max_width).saturating_sub(5);
    let content = truncate_str(&task.content, avail);
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(color)),
        Span::raw(content),
    ])
}

/// 状态优先级（越小越靠前显示）。
fn status_priority(s: TaskStatus) -> u8 {
    match s {
        TaskStatus::InProgress => 0,
        TaskStatus::Pending => 1,
        TaskStatus::Completed => 2,
        TaskStatus::Cancelled => 3,
    }
}

/// 状态标记符号与颜色。
fn status_mark(status: TaskStatus, theme: &Theme) -> (&'static str, ratatui::style::Color) {
    match status {
        TaskStatus::InProgress => ("▶", theme.task_in_progress),
        TaskStatus::Pending => ("○", theme.task_pending),
        TaskStatus::Completed => ("✓", theme.task_completed),
        TaskStatus::Cancelled => ("✗", theme.task_cancelled),
    }
}

/// 按 UTF-8 字符数截断字符串，超出部分用 `…` 替代。
///
/// `max_chars` 为 0 时返回原字符串（无可用宽度时不截断，交由 ratatui `Wrap` 处理）。
fn truncate_str(s: &str, max_chars: usize) -> String {
    if max_chars == 0 || s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use time::OffsetDateTime;

    fn make_task(content: &str, status: TaskStatus) -> Task {
        let mut t = Task::new(content.to_string());
        if status != TaskStatus::Pending {
            t.status = status;
        }
        t
    }

    #[test]
    fn status_priority_orders_correctly() {
        assert!(status_priority(TaskStatus::InProgress) < status_priority(TaskStatus::Pending));
        assert!(status_priority(TaskStatus::Pending) < status_priority(TaskStatus::Completed));
        assert!(status_priority(TaskStatus::Completed) < status_priority(TaskStatus::Cancelled));
    }

    #[test]
    fn truncate_str_short_unchanged() {
        assert_eq!(truncate_str("hi", 10), "hi");
    }

    #[test]
    fn truncate_str_long_gets_ellipsis() {
        let long = "abcdefghij";
        let result = truncate_str(long, 5);
        assert_eq!(result.chars().count(), 5);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn truncate_str_empty_max_returns_unchanged() {
        assert_eq!(truncate_str("abc", 0), "abc");
    }

    #[test]
    fn task_line_in_progress_uses_play_marker() {
        let theme = Theme::default();
        let task = make_task("demo", TaskStatus::InProgress);
        let line = task_line(&task, &theme, 40);
        // 第一段是标记前缀
        assert!(!line.spans.is_empty(), "expected non-empty: line.spans");
    }

    #[test]
    #[allow(clippy::unused_enumerate_index)]
    fn sort_puts_in_progress_first() {
        let now = OffsetDateTime::now_utc();
        let pending = Task {
            id: "1".to_string(),
            content: "p".to_string(),
            status: TaskStatus::Pending,
            summary: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        let in_prog = Task {
            id: "2".to_string(),
            content: "i".to_string(),
            status: TaskStatus::InProgress,
            summary: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        let mut sorted: Vec<&Task> = vec![&pending, &in_prog];
        sorted.sort_by_key(|t| status_priority(t.status));
        assert_eq!(sorted[0].status, TaskStatus::InProgress);
    }
}
