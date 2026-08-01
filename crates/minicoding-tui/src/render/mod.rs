//! 渲染辅助模块。
//!
//! - [`markdown`]：流式 Markdown → `ratatui::text::Line` 的转换。
//! - [`theme`]：集中配色方案，视图模块按语义取色，便于全局切换主题。

pub mod markdown;
pub mod theme;
