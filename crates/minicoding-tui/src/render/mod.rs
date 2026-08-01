//! 渲染辅助模块（T-M7-2/T-M7-4）。
//!
//! - [`markdown`]：流式 Markdown → `ratatui::text::Line` 的转换（T-M7-2）。
//! - [`theme`]：集中配色方案（T-M7-4），视图模块按语义取色，便于全局切换主题。

pub mod markdown;
pub mod theme;
