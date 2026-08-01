//! # minicoding-tui
//!
//! TUI frontend（M7）：基于 `ratatui` 的全屏交互界面。
//!
//! 多会话、工具调用面板、权限弹窗、流式 Markdown 渲染。独立线程跑 `Runtime`，UI 线程
//! 通过 channel 收发事件。权限弹窗非阻塞：`Runtime` 在 `Verdict::Ask` 时通过
//! `TuiPrompter`（点对点）挂起该工具调用，UI 处理后回传 `Decision`。
//!
//! ## 线程模型
//!
//! - **主线程**：ratatui 同步事件循环（`draw` + `crossterm::event::poll`），100ms 窗口
//!   轮询，非阻塞消费 Runtime 事件（`try_recv`）。
//! - **tokio runtime**（多线程）：`spawn_runtime_bridge` 启动两个 task：
//!   - event forwarder：`EventBus` → [`AppEvent::Runtime`]；
//!   - command handler：[`UiCommand`] → `run_turn` → [`AppEvent::TurnResult`]。
//!
//! 详见 `docs/modules.md` §13、`docs/roadmap.md` M7、`docs/design.md` §25。

#![deny(clippy::all, clippy::pedantic)]

pub mod app;
pub mod event;
pub mod render;
pub mod runtime_bridge;
pub mod view;

pub use app::{App, InputState};
pub use event::AppEvent;
pub use runtime_bridge::{UiCommand, spawn_runtime_bridge};
