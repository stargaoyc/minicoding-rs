//! 事件类型：统一终端事件与 Runtime 事件（T-M7-1）。
//!
//! TUI 主循环以 100ms 为窗口轮询 [`AppEvent`]：
//! - [`AppEvent::Term`]：来自 crossterm 的终端事件（按键/鼠标/ resize）；
//! - [`AppEvent::Runtime`]：转发自 `EventBus` 的运行时事件（token 流、工具调用等）；
//! - [`AppEvent::TurnResult`]：`run_turn` 完成结果；
//! - [`AppEvent::PermissionRequest`]：`TuiPrompter` 点对点发来的权限询问（T-M7-3），
//!   UI 渲染弹窗后通过 `reply` 回传 `Decision`，Runtime 侧挂起的工具调用继续/中止。
//!
//! 权限交互走点对点 oneshot，不通过 broadcast 总线（见 `design.md` §9.1/§9.2）。

use minicoding_core::model::TurnOutcome;
use minicoding_core::policy::TuiPermissionRequest;
use minicoding_core::runtime::Event as RuntimeEvent;

/// TUI 主循环消费的统一事件。
#[derive(Debug)]
pub enum AppEvent {
    /// 终端事件（crossterm）。
    Term(crossterm::event::Event),
    /// Runtime 事件转发（token/工具调用/会话/权限通知等）。
    Runtime(RuntimeEvent),
    /// 一轮对话完成（`run_turn` 返回）。
    TurnResult(Result<TurnOutcome, String>),
    /// 权限询问（点对点，T-M7-3）：包装 [`TuiPermissionRequest`]，UI 渲染弹窗后
    /// 通过 `reply` 回传 `Decision`，Runtime 侧挂起的工具调用继续/中止。
    PermissionRequest(TuiPermissionRequest),
    /// 切换会话请求（T-M7-2）：用户在侧栏选中并按 Enter，bridge 取消当前 turn
    /// 后回传此事件，main.rs 重建 Runtime（`SessionLoadMode::Resume`）实现切换。
    SwitchSession(String),
    /// `/summary` 结果（R8）：bridge 调 `Runtime::summarize_session` 后回传
    /// 摘要文本（`None` = 无消息/未注入 summarizer），UI 渲染为 System 行。
    Summary(Option<String>),
}
