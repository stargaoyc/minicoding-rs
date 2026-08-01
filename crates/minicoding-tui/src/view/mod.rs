//! 视图组件模块（T-M7-2/T-M7-4）。
//!
//! 把 `App` 状态渲染拆分到独立视图模块，避免 `app.rs` 膨胀。每个视图是一个
//! 自由函数 `render_xxx(frame, area, state)`，由 `App::render` 编排调用。
//!
//! - [`chat`]：对话主视图（流式 Markdown + 历史 + 工具调用行）
//! - [`sidebar`]：多会话侧栏（当前会话高亮 + 最近会话列表）
//! - [`tool_panel`]：工具调用进度面板（T-M7-4）
//! - [`task_panel`]：任务列表面板（订阅 `Event::TaskUpdated`，T-M7-4）

pub mod chat;
pub mod sidebar;
pub mod task_panel;
pub mod tool_panel;
