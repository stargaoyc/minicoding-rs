//! 主题配色（T-M7-4）。
//!
//! 集中定义 TUI 各组件的颜色与样式，避免散落在各视图模块中硬编码 `Color::xxx`。
//! 切换主题只需替换 [`Theme`] 实例（未来可通过配置文件 / `Ctrl+T` 切换）。
//!
//! ## 设计
//!
//! - [`Theme`] 是纯数据结构，所有字段为 `Color` / `Modifier`，`Clone + Copy`。
//! - [`Theme::default`] 为深色主题（终端友好）；[`Theme::light`] 为浅色主题。
//! - 视图模块接收 `&Theme` 参数，从中取色，不再直接引用 `Color::Green` 等。
//! - 当前 M7 骨架只提供两套预设；`App` 持有 `Theme`，未来 `Ctrl+T` 切换。

use ratatui::style::{Color, Modifier, Style};

/// TUI 主题（配色方案）。
///
/// 所有颜色集中在此结构体，视图模块按语义取色（如 `theme.user_msg`），
/// 不直接硬编码 `Color::xxx`，便于全局切换主题。
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// 标题栏背景色。
    pub titlebar_bg: Color,
    /// 标题栏文字色。
    pub titlebar_fg: Color,
    /// 状态指示器（就绪）色。
    pub status_idle: Color,
    /// 状态指示器（生成中）色。
    pub status_busy: Color,
    /// 用户消息前缀色。
    pub user_prefix: Color,
    /// 助手消息前缀色。
    pub assistant_prefix: Color,
    /// 工具调用前缀色。
    pub tool_prefix: Color,
    /// 系统消息前缀色。
    pub system_prefix: Color,
    /// 工具调用完成标记色（✓）。
    pub tool_done: Color,
    /// 工具调用进行中标记色（…）。
    pub tool_pending: Color,
    /// 任务 `InProgress` 状态色。
    pub task_in_progress: Color,
    /// 任务 `Pending` 状态色。
    pub task_pending: Color,
    /// 任务 `Completed` 状态色。
    pub task_completed: Color,
    /// 任务 `Cancelled` 状态色。
    pub task_cancelled: Color,
    /// 侧栏当前会话高亮背景色。
    pub sidebar_current_bg: Color,
    /// 次要文字色（时间戳、提示等）。
    pub muted: Color,
    /// Markdown 标题色（H1）。
    pub md_h1: Color,
    /// Markdown 标题色（H2）。
    pub md_h2: Color,
    /// Markdown 标题色（H3）。
    pub md_h3: Color,
    /// Markdown 代码块边框色。
    pub md_code_border: Color,
    /// 权限弹窗低风险色。
    pub risk_low: Color,
    /// 权限弹窗中风险色。
    pub risk_medium: Color,
    /// 权限弹窗高风险色。
    pub risk_high: Color,
}

impl Theme {
    /// 深色主题（默认，终端友好）。
    #[must_use]
    pub fn dark() -> Self {
        Self {
            titlebar_bg: Color::Black,
            titlebar_fg: Color::White,
            status_idle: Color::Green,
            status_busy: Color::Yellow,
            user_prefix: Color::Cyan,
            assistant_prefix: Color::Green,
            tool_prefix: Color::Magenta,
            system_prefix: Color::DarkGray,
            tool_done: Color::Green,
            tool_pending: Color::Yellow,
            task_in_progress: Color::Yellow,
            task_pending: Color::DarkGray,
            task_completed: Color::Green,
            task_cancelled: Color::Red,
            sidebar_current_bg: Color::DarkGray,
            muted: Color::DarkGray,
            md_h1: Color::Blue,
            md_h2: Color::Cyan,
            md_h3: Color::Magenta,
            md_code_border: Color::DarkGray,
            risk_low: Color::Green,
            risk_medium: Color::Yellow,
            risk_high: Color::Red,
        }
    }

    /// 浅色主题（亮色终端）。
    #[must_use]
    pub fn light() -> Self {
        Self {
            titlebar_bg: Color::White,
            titlebar_fg: Color::Black,
            status_idle: Color::Green,
            status_busy: Color::Yellow,
            user_prefix: Color::Blue,
            assistant_prefix: Color::Green,
            tool_prefix: Color::Magenta,
            system_prefix: Color::Gray,
            tool_done: Color::Green,
            tool_pending: Color::Yellow,
            task_in_progress: Color::Yellow,
            task_pending: Color::Gray,
            task_completed: Color::Green,
            task_cancelled: Color::Red,
            sidebar_current_bg: Color::Gray,
            muted: Color::Gray,
            md_h1: Color::Blue,
            md_h2: Color::Cyan,
            md_h3: Color::Magenta,
            md_code_border: Color::Gray,
            risk_low: Color::Green,
            risk_medium: Color::Yellow,
            risk_high: Color::Red,
        }
    }

    /// 状态指示器样式。
    #[must_use]
    pub fn status_indicator(&self, is_turning: bool) -> Style {
        let color = if is_turning {
            self.status_busy
        } else {
            self.status_idle
        };
        Style::default().fg(color)
    }

    /// 用户消息前缀样式。
    #[must_use]
    pub fn user_prefix_style(&self) -> Style {
        Style::default().fg(self.user_prefix)
    }

    /// 助手消息前缀样式。
    #[must_use]
    pub fn assistant_prefix_style(&self) -> Style {
        Style::default().fg(self.assistant_prefix)
    }

    /// 工具调用前缀样式。
    #[must_use]
    pub fn tool_prefix_style(&self) -> Style {
        Style::default().fg(self.tool_prefix)
    }

    /// 系统消息前缀样式。
    #[must_use]
    pub fn system_prefix_style(&self) -> Style {
        Style::default().fg(self.system_prefix)
    }

    /// 工具调用标记样式（按是否完成取色）。
    #[must_use]
    pub fn tool_mark_style(&self, done: bool) -> Style {
        Style::default().fg(if done {
            self.tool_done
        } else {
            self.tool_pending
        })
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// 加粗样式工具函数。
#[must_use]
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// 暗淡样式工具函数。
#[must_use]
pub fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn dark_theme_default() {
        let t = Theme::default();
        assert_eq!(t.status_idle, Theme::dark().status_idle);
    }

    #[test]
    fn light_theme_differs_from_dark() {
        assert_ne!(Theme::light().titlebar_bg, Theme::dark().titlebar_bg);
    }

    #[test]
    fn status_indicator_reflects_turning() {
        let t = Theme::default();
        let idle = t.status_indicator(false);
        let busy = t.status_indicator(true);
        assert_ne!(idle, busy);
    }

    #[test]
    fn tool_mark_style_switches_on_done() {
        let t = Theme::default();
        let done_style = t.tool_mark_style(true);
        let pending_style = t.tool_mark_style(false);
        assert_ne!(done_style, pending_style);
    }
}
