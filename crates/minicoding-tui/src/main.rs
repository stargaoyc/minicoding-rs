//! # minicoding-tui
//!
//! `TUI` frontend（M7）：基于 `ratatui` 的全屏交互界面。
//!
//! 多会话、工具调用面板、权限弹窗、流式 Markdown 渲染。独立线程跑 `Runtime`，UI 线程
//! 通过 channel 收发事件。权限弹窗非阻塞：`Runtime` 在 `Verdict::Ask` 时通过
//! `TuiPrompter`（点对点）挂起该工具调用，UI 处理后回传 `Decision`。
//!
//! 当前 M0 阶段：仅占位骨架（T-M0-1），实现见 M7。
//!
//! 详见 `docs/modules.md` §13、`docs/roadmap.md` M7。

#![deny(clippy::all, clippy::pedantic)]

fn main() {
    // M0 占位：TUI 实现见 M7
    println!("minicoding-tui - terminal UI (skeleton, M7)");
}
