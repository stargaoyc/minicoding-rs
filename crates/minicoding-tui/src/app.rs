//! App 状态机与渲染（T-M7-1）。
//!
//! [`App`] 持有聊天历史、流式 token 缓冲、输入状态、权限弹窗状态，消费 [`AppEvent`]
//! 更新状态，由 `render` 绘制到 `ratatui::Frame`。
//!
//! ## 布局
//!
//! ```text
//! ┌──────────────────────────────┐
//! │ minicoding-tui  [● idle/turn]│  标题栏（状态指示）
//! ├──────────────────────────────┤
//! │ You: ...                     │
//! │ Assistant: ...               │  聊天区（自动滚到底部）
//! │ [streaming token...]         │
//! ├──────────────────────────────┤
//! │ > input_                     │  输入框（带光标）
//! └──────────────────────────────┘
//! ```
//!
//! 权限弹窗（T-M7-3）以覆盖层渲染在中心区域。

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use minicoding_core::model::{Role, Task};
use minicoding_core::policy::{Decision, PermissionPrompt, PromptOption, TuiPermissionRequest};
use minicoding_core::runtime::Event as RuntimeEvent;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::event::AppEvent;
use crate::render::theme::Theme;
use crate::runtime_bridge::UiCommand;
// `view::chat` 与 `view::sidebar` 由 `render` 编排调用；此处仅 re-export `ChatLine`
// 供视图模块引用。

/// 聊天历史中的一行。
#[derive(Debug, Clone)]
pub enum ChatLine {
    /// 用户消息。
    User(String),
    /// 助手消息（走 Markdown 解析，T-M7-2）。
    Assistant(String),
    /// 工具调用行：`tool` 名称，`done` 标识是否完成。
    Tool { tool: String, done: bool },
    /// 系统消息。
    System(String),
}

/// 待处理的权限询问（T-M7-3）。
#[derive(Debug)]
struct PendingPermission {
    prompt: PermissionPrompt,
    reply: oneshot::Sender<Decision>,
}

impl From<TuiPermissionRequest> for PendingPermission {
    fn from(req: TuiPermissionRequest) -> Self {
        Self {
            prompt: req.prompt,
            reply: req.reply,
        }
    }
}

/// 输入缓冲状态：字符插入/光标移动/历史切换。
///
/// `cursor` 为字节偏移，始终对齐 UTF-8 字符边界（通过 `char_indices` 计算）。
/// 不引入 `reedline`：reedline 接管 stdin/stdout 行渲染，与 ratatui 全屏
/// alternate screen 模式冲突（见 `design.md` §25 权衡）。
#[derive(Debug, Default)]
pub struct InputState {
    buffer: String,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
}

impl InputState {
    /// 插入一个字符到光标处。
    fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// 删除光标前一个字符（Backspace）。
    fn backspace(&mut self) {
        if let Some((idx, _)) = self.buffer[..self.cursor].char_indices().next_back() {
            self.buffer.replace_range(idx..self.cursor, "");
            self.cursor = idx;
        }
    }

    /// 光标左移一个字符。
    fn cursor_left(&mut self) {
        if let Some((idx, _)) = self.buffer[..self.cursor].char_indices().next_back() {
            self.cursor = idx;
        }
    }

    /// 光标右移一个字符。
    fn cursor_right(&mut self) {
        if let Some(c) = self.buffer[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    /// 光标移到行首。
    fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    /// 光标移到行尾。
    fn cursor_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    /// 切换到上一条历史。
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => self.history.len() - 1,
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_idx = Some(idx);
        self.buffer = self.history[idx].clone();
        self.cursor_end();
    }

    /// 切换到下一条历史。
    fn history_next(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) if i + 1 >= self.history.len() => {
                self.history_idx = None;
                self.buffer.clear();
                self.cursor = 0;
            }
            Some(i) => {
                self.history_idx = Some(i + 1);
                self.buffer = self.history[i + 1].clone();
                self.cursor_end();
            }
        }
    }

    /// 提交当前输入：返回文本，清空缓冲，记入历史。
    fn submit(&mut self) -> Option<String> {
        if self.buffer.trim().is_empty() {
            return None;
        }
        let text = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.history_idx = None;
        // 避免连续重复历史
        if self.history.last().is_none_or(|h| h != &text) {
            self.history.push(text.clone());
        }
        Some(text)
    }

    /// 光标在缓冲中的显示列（字符数，非字节数）。
    fn cursor_col(&self) -> usize {
        self.buffer[..self.cursor].chars().count()
    }
}

/// 底部面板模式（T-M7-4）。
///
/// F3 切换工具面板，Ctrl+T 切换任务面板（见 `design.md` §18.4）。
/// 两者互斥：同一底部区域只显示一种面板，避免布局碎片化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelMode {
    /// 不显示底部面板。
    #[default]
    Off,
    /// 工具调用进度面板（F3）。
    Tool,
    /// 任务列表面板（Ctrl+T，订阅 `Event::TaskUpdated`）。
    Task,
}

/// TUI 应用状态机。
pub struct App {
    ui_tx: mpsc::Sender<UiCommand>,
    lines: Vec<ChatLine>,
    streaming: String,
    /// FE-8（2026-08-25 R2 审查）：reasoning 增量缓冲（turn 内累计，固化时清除）
    reasoning: String,
    input: InputState,
    is_turning: bool,
    should_exit: bool,
    pending_permission: Option<PendingPermission>,
    status_msg: String,
    // T-M7-2：多会话侧栏
    sessions: Vec<minicoding_core::storage::SessionListItem>,
    current_session_id: String,
    sidebar_visible: bool,
    sidebar_state: ratatui::widgets::ListState,
    // T-M7-2：用户请求切换到的会话 ID（main.rs 读取后重建 Runtime）
    pending_switch: Option<String>,
    // T-M7-4：底部面板模式（工具/任务/关，互斥）
    panel_mode: PanelMode,
    // T-M7-4：任务列表（由 `Event::TaskUpdated` 维护）
    tasks: Vec<Task>,
    task_panel_state: ratatui::widgets::ListState,
    /// 当前 Runtime 的取消 token（2026-08-23 审查 §11-P0：Ctrl-C 中断 turn——
    /// 此前仅设状态文案不调 cancel，长 turn 唯一手段是杀进程）。turn 间隙为
    /// `None`（`cancel()` 对非运行 turn 本就不生效）。
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    /// 回看偏移（0=吸底；PgUp 增/PgDn 减，2026-08-23 审查遗留#4 scrollback）
    scroll_offset: usize,
    // T-M7-4：主题配色（未来 Ctrl+Shift+T 切换深/浅色）
    theme: Theme,
}

impl App {
    /// 创建 App，持有发送给 Runtime 桥接的 channel sender。
    ///
    /// `sessions` 为最近会话列表（按 `last_message_at` 倒序），用于侧栏显示；
    /// `current_session_id` 为当前会话 ID（高亮）。
    #[must_use]
    pub fn new(
        ui_tx: mpsc::Sender<UiCommand>,
        sessions: Vec<minicoding_core::storage::SessionListItem>,
        current_session_id: String,
    ) -> Self {
        let mut sidebar_state = ratatui::widgets::ListState::default();
        if !sessions.is_empty() {
            sidebar_state.select(Some(0));
        }
        Self {
            ui_tx,
            lines: Vec::new(),
            streaming: String::new(),
            reasoning: String::new(),
            input: InputState::default(),
            is_turning: false,
            should_exit: false,
            pending_permission: None,
            status_msg: "就绪".to_string(),
            sessions,
            current_session_id,
            sidebar_visible: false,
            sidebar_state,
            pending_switch: None,
            panel_mode: PanelMode::Off,
            tasks: Vec::new(),
            task_panel_state: ratatui::widgets::ListState::default(),
            cancel_token: None,
            scroll_offset: 0,
            theme: Theme::default(),
        }
    }

    /// 是否应退出主循环。
    #[must_use]
    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// 取出用户请求切换到的会话 ID（T-M7-2）。
    ///
    /// main.rs 在 `run_app` 返回后调用：若返回 `Some(id)`，重建 Runtime 以
    /// `SessionLoadMode::Resume(id)` 重新进入主循环。
    #[must_use]
    pub fn take_pending_switch(&mut self) -> Option<String> {
        self.pending_switch.take()
    }

    /// 处理 Runtime 桥接/终端事件，更新状态。
    pub fn handle_app_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Term(term_ev) => self.handle_term_event(&term_ev),
            AppEvent::Runtime(rt_ev) => self.handle_runtime_event(rt_ev),
            AppEvent::TurnResult(result) => {
                self.is_turning = false;
                match result {
                    Ok(minicoding_core::model::TurnOutcome::Finished(_)) => {
                        self.status_msg = "就绪".to_string();
                    }
                    Ok(minicoding_core::model::TurnOutcome::Interrupted(_)) => {
                        self.status_msg = "已中断".to_string();
                        self.streaming.clear();
                    }
                    Ok(minicoding_core::model::TurnOutcome::Failed(e)) => {
                        self.status_msg = format!("失败: {e}");
                        self.streaming.clear();
                    }
                    Err(e) => {
                        self.status_msg = format!("运行时错误: {e}");
                        self.streaming.clear();
                    }
                }
            }
            AppEvent::PermissionRequest(req) => {
                // T-M7-3：存入 pending，UI 渲染弹窗等待用户按键回传 Decision。
                // Runtime 侧 `TuiPrompter::prompt` 在 await oneshot receiver，工具调用挂起。
                self.pending_permission = Some(PendingPermission::from(req));
            }
            AppEvent::SwitchSession(id) => {
                // T-M7-2：bridge 已取消当前 turn，设置 pending_switch 并退出主循环，
                // main.rs 重建 Runtime（`SessionLoadMode::Resume`）后重新进入。
                self.pending_switch = Some(id);
                self.should_exit = true;
            }
        }
    }

    /// 处理 Runtime 事件（EventBus 转发）。
    fn handle_runtime_event(&mut self, ev: RuntimeEvent) {
        match ev {
            RuntimeEvent::TurnStreamingStarted => {
                self.is_turning = true;
                self.status_msg = "生成中…".to_string();
            }
            RuntimeEvent::Token(text) => {
                self.streaming.push_str(&text);
            }
            RuntimeEvent::ReasoningDelta(text) => {
                // FE-8（2026-08-25 R2 审查）：reasoning 增量进独立缓冲——此前
                // TUI 丢弃该事件，思考过程仅 Web/SDK 可见。以暗色 System 行
                // 前缀区分正文（不固化，turn 结束时随 streaming 一起落行）。
                self.reasoning.push_str(&text);
            }
            RuntimeEvent::MessageAppended(msg) => {
                // 流式累积固化为消息；若为 assistant 文本，先落 streaming 再覆盖
                if !self.reasoning.is_empty() {
                    self.reasoning.clear();
                }
                if !self.streaming.is_empty() && msg.role == Role::Assistant {
                    self.lines
                        .push(ChatLine::Assistant(std::mem::take(&mut self.streaming)));
                }
                match msg.role {
                    Role::User => {
                        let text = msg.text();
                        if !text.is_empty() {
                            self.scroll_offset = 0;
                            self.lines.push(ChatLine::User(text));
                        }
                    }
                    Role::Assistant => {
                        // 已从 streaming 落入；若有非文本块或 streaming 为空，补一条
                        let text = msg.text();
                        if !text.is_empty()
                            && self
                                .lines
                                .last()
                                .is_none_or(|l| !matches!(l, ChatLine::Assistant(_)))
                        {
                            self.lines.push(ChatLine::Assistant(text));
                        }
                    }
                    Role::Tool => {
                        let text = msg.text();
                        if !text.is_empty() {
                            self.lines.push(ChatLine::Tool {
                                tool: text,
                                done: true,
                            });
                        }
                    }
                    Role::System => {
                        let text = msg.text();
                        if !text.is_empty() {
                            self.lines.push(ChatLine::System(text));
                        }
                    }
                }
            }
            RuntimeEvent::ToolCallStarted { tool, .. } => {
                self.lines.push(ChatLine::Tool { tool, done: false });
            }
            RuntimeEvent::ToolCallFinished { .. } => {
                // 标记最后一个同名 Tool 行为 done
                if let Some(ChatLine::Tool { done, .. }) = self.lines.last_mut() {
                    *done = true;
                }
            }
            RuntimeEvent::TurnEnd { .. } => {
                self.is_turning = false;
                self.reasoning.clear();
                if !self.streaming.is_empty() {
                    self.lines
                        .push(ChatLine::Assistant(std::mem::take(&mut self.streaming)));
                }
                self.status_msg = "就绪".to_string();
            }
            RuntimeEvent::TaskUpdated { task } => {
                // T-M7-4：更新任务列表（按 task_id upsert），UI 下次 draw 刷新。
                match self.tasks.iter_mut().find(|t| t.id == task.id) {
                    Some(existing) => *existing = task,
                    None => self.tasks.push(task),
                }
                // 若任务面板未显示，切换到 Task 模式让用户看到进度
                if self.panel_mode == PanelMode::Off {
                    self.panel_mode = PanelMode::Task;
                }
            }
            _ => {}
        }
    }

    /// 处理终端按键事件。
    fn handle_term_event(&mut self, ev: &crossterm::event::Event) {
        // 权限弹窗激活时，按键仅用于回弹窗
        if self.pending_permission.is_some() {
            if let crossterm::event::Event::Key(key) = ev {
                self.handle_permission_key(key);
            }
            return;
        }
        if let crossterm::event::Event::Key(key) = ev {
            // 侧栏激活时，按键优先用于会话列表导航（F2/Esc 切回输入模式）
            if self.sidebar_visible {
                self.handle_sidebar_key(key);
            } else {
                self.handle_key(key);
            }
        }
    }

    /// 注入当前 Runtime 的取消 token（build 后调用；切换会话重建 Runtime 后需重设）。
    pub fn set_cancel_token(&mut self, token: tokio_util::sync::CancellationToken) {
        self.cancel_token = Some(token);
    }

    /// 注入恢复会话的历史消息（§11-P1：此前 `restore_history` 只回填上下文，
    /// UI 聊天区从空白开始，会话切换形同虚设）。
    pub fn set_history(&mut self, history: Vec<ChatLine>) {
        self.lines = history;
    }

    /// 处理普通按键（输入模式）。
    fn handle_key(&mut self, key: &KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Enter, _) => self.submit_input(),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if self.is_turning {
                    // 中断当前 turn（C-13 graceful：已落盘消息保留，
                    // run_turn 返回 Interrupted）——2026-08-23 审查 §11-P0
                    if let Some(token) = &self.cancel_token {
                        token.cancel();
                    }
                    self.status_msg = "正在中断…".to_string();
                } else {
                    self.should_exit = true;
                }
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => self.should_exit = true,
            // scrollback（遗留#4）：PgUp 上翻 / PgDn 下翻（减到 0 即吸底）
            (KeyCode::PageUp, _) => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                self.status_msg = format!("↑ 回看 {} 行", self.scroll_offset);
            }
            (KeyCode::PageDown, _) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                self.status_msg = if self.scroll_offset == 0 {
                    "↓ 已回到底部".to_string()
                } else {
                    format!("↑ 回看 {} 行", self.scroll_offset)
                };
            }
            // T-M7-2：F2 切换侧栏显示
            (KeyCode::F(2), _) => {
                self.sidebar_visible = true;
                self.status_msg = "会话列表（↑↓ 选择 / Enter 恢复 / Esc 返回）".to_string();
            }
            // T-M7-4：F3 切换工具面板（与任务面板互斥）
            (KeyCode::F(3), _) => {
                self.panel_mode = if self.panel_mode == PanelMode::Tool {
                    PanelMode::Off
                } else {
                    PanelMode::Tool
                };
            }
            // T-M7-4：Ctrl+T 切换任务面板（见 design.md §18.4）
            (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                self.panel_mode = if self.panel_mode == PanelMode::Task {
                    PanelMode::Off
                } else {
                    PanelMode::Task
                };
            }
            // T-M7-4：F4 切换主题（深/浅色）
            (KeyCode::F(4), _) => {
                self.theme = if self.theme.titlebar_bg == Color::Black {
                    Theme::light()
                } else {
                    Theme::dark()
                };
                self.status_msg = "主题已切换".to_string();
            }
            (KeyCode::Up, KeyModifiers::NONE) => self.input.history_prev(),
            (KeyCode::Down, KeyModifiers::NONE) => self.input.history_next(),
            (KeyCode::Left, _) => self.input.cursor_left(),
            (KeyCode::Right, _) => self.input.cursor_right(),
            (KeyCode::Home, _) => self.input.cursor_home(),
            (KeyCode::End, _) => self.input.cursor_end(),
            (KeyCode::Backspace, _) => self.input.backspace(),
            (KeyCode::Delete, _) => {
                // 删除光标处字符
                if self.input.cursor < self.input.buffer.len() {
                    self.input.cursor_right();
                    self.input.backspace();
                }
            }
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                self.input.insert_char(c);
            }
            _ => {}
        }
    }

    /// 处理侧栏按键（T-M7-2）。
    fn handle_sidebar_key(&mut self, key: &KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::F(2) => {
                self.sidebar_visible = false;
                self.status_msg = "就绪".to_string();
            }
            KeyCode::Up => {
                let len = self.sessions.len();
                if len > 0 {
                    let cur = self.sidebar_state.selected().unwrap_or(0);
                    let next = if cur == 0 { len - 1 } else { cur - 1 };
                    self.sidebar_state.select(Some(next));
                }
            }
            KeyCode::Down => {
                let len = self.sessions.len();
                if len > 0 {
                    let cur = self.sidebar_state.selected().unwrap_or(0);
                    let next = if cur + 1 >= len { 0 } else { cur + 1 };
                    self.sidebar_state.select(Some(next));
                }
            }
            KeyCode::Enter => {
                // 恢复选中的会话（发送 SwitchSession 给 bridge）
                if let Some(idx) = self.sidebar_state.selected()
                    && let Some(meta) = self.sessions.get(idx)
                {
                    if meta.id.as_str() == self.current_session_id {
                        // 已是当前会话，仅关闭侧栏
                        self.sidebar_visible = false;
                        self.status_msg = "已是当前会话".to_string();
                    } else {
                        let id = meta.id.clone();
                        let _ = self.ui_tx.try_send(UiCommand::SwitchSession(id));
                        self.status_msg = "正在切换会话…".to_string();
                        // 不立即关闭侧栏：等 SwitchSession 事件回来再退出主循环
                    }
                }
            }
            _ => {}
        }
    }

    /// 处理权限弹窗按键（T-M7-3）：y/a 允许，n/d 拒绝，Esc 拒绝。
    fn handle_permission_key(&mut self, key: &KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let pending = self.pending_permission.take();
        let Some(p) = pending else { return };
        // 遗留#3：`a` 仅在 prompt 提供 AllowAlways 选项时映射 Always
        // （C-23：受保护文件的 restricted ask 不含该选项，`a` 退化为一次性 Allow）
        let always_offered = p.prompt.options.contains(&PromptOption::AllowAlways);
        let decision = match (key.code, always_offered) {
            (KeyCode::Char('a'), true) => Decision::AllowAlways,
            (KeyCode::Char('y' | 'Y'), _) | (KeyCode::Char('a' | 'A'), false) => Decision::Allow,
            (KeyCode::Char('n' | 'N') | KeyCode::Esc, _) => Decision::Deny("用户拒绝".to_string()),
            _ => {
                // 未识别按键，放回 pending 等待下次按键
                self.pending_permission = Some(p);
                return;
            }
        };
        let _ = p.reply.send(decision);
    }

    /// 提交输入：发送给 Runtime 桥接，加入历史。
    fn submit_input(&mut self) {
        if self.is_turning {
            return; // 一轮未结束，忽略提交
        }
        if let Some(text) = self.input.submit() {
            // 立即在 UI 显示用户消息（不等待 MessageAppended 事件，提升响应感）
            self.scroll_offset = 0;
            self.lines.push(ChatLine::User(text.clone()));
            let _ = self.ui_tx.try_send(UiCommand::Submit(text));
            self.is_turning = true;
            self.status_msg = "等待响应…".to_string();
        }
    }

    /// 渲染到 frame。
    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        // 主体：标题栏 + 中间区 + 输入框
        let main_chunks = Layout::vertical([
            Constraint::Length(1), // 标题栏
            Constraint::Min(1),    // 中间区（侧栏 + 对话 / 工具面板）
            Constraint::Length(3), // 输入框
        ])
        .split(area);

        self.render_titlebar(frame, main_chunks[0]);

        // 中间区：按需切分为 侧栏 + 对话（+ 工具/任务面板，T-M7-4）
        let middle = main_chunks[1];
        if self.sidebar_visible {
            // 侧栏 24 列 + 对话区
            let middle_chunks = Layout::horizontal([
                Constraint::Length(24), // 侧栏
                Constraint::Min(1),     // 对话 + 面板
            ])
            .split(middle);
            crate::view::sidebar::render_sidebar(
                frame,
                middle_chunks[0],
                &self.sessions,
                &self.current_session_id,
                &mut self.sidebar_state,
            );
            self.render_chat_with_panel(frame, middle_chunks[1]);
        } else {
            self.render_chat_with_panel(frame, middle);
        }

        self.render_input(frame, main_chunks[2]);

        // 权限弹窗覆盖层（T-M7-3）
        if let Some(pending) = &self.pending_permission {
            Self::render_permission_popup(frame, area, pending);
        }
    }

    /// 渲染对话区 + 可选底部面板（工具/任务，T-M7-4）。
    ///
    /// `panel_mode` 为 `Off` 时对话区占满；否则底部预留 8 行给面板。
    fn render_chat_with_panel(&mut self, frame: &mut Frame, area: Rect) {
        match self.panel_mode {
            PanelMode::Off => {
                crate::view::chat::render_chat(
                    frame,
                    area,
                    &self.lines,
                    &self.streaming,
                    &self.reasoning,
                    self.scroll_offset,
                );
            }
            PanelMode::Tool => {
                let chunks = Layout::vertical([
                    Constraint::Min(1),    // 对话
                    Constraint::Length(8), // 工具面板
                ])
                .split(area);
                crate::view::chat::render_chat(
                    frame,
                    chunks[0],
                    &self.lines,
                    &self.streaming,
                    &self.reasoning,
                    self.scroll_offset,
                );
                crate::view::tool_panel::render_tool_panel(frame, chunks[1], &self.lines);
            }
            PanelMode::Task => {
                let chunks = Layout::vertical([
                    Constraint::Min(1),    // 对话
                    Constraint::Length(8), // 任务面板
                ])
                .split(area);
                crate::view::chat::render_chat(
                    frame,
                    chunks[0],
                    &self.lines,
                    &self.streaming,
                    &self.reasoning,
                    self.scroll_offset,
                );
                crate::view::task_panel::render_task_panel(
                    frame,
                    chunks[1],
                    &self.tasks,
                    &mut self.task_panel_state,
                    &self.theme,
                );
            }
        }
    }

    fn render_titlebar(&self, frame: &mut Frame, area: Rect) {
        let indicator = if self.is_turning {
            "● 生成中"
        } else {
            "○ 就绪"
        };
        let indicator_color = if self.is_turning {
            Color::Yellow
        } else {
            Color::Green
        };
        // 标题栏显示当前会话 ID 前 8 字符（T-M7-2）
        let session_short: String = if self.current_session_id.len() >= 8 {
            self.current_session_id[..8].to_string()
        } else {
            self.current_session_id.clone()
        };
        let title = Line::from(vec![
            Span::styled(
                " minicoding-tui ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(indicator, Style::default().fg(indicator_color)),
            Span::raw("  "),
            Span::styled(
                format!("[{session_short}] "),
                Style::default().fg(Color::Blue),
            ),
            Span::styled(
                self.status_msg.as_str(),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::styled(
                "[F2 会话] [F3 工具] [Ctrl+T 任务] [F4 主题]",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        let block = Block::default().style(Style::default().bg(Color::Black));
        let para = Paragraph::new(title).block(block);
        frame.render_widget(para, area);
    }

    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" 输入（Enter 发送 / ↑↓ 历史 / F2 会话 / F3 工具 / Ctrl+T 任务 / F4 主题 / Ctrl-C 退出）");
        let para = Paragraph::new(self.input.buffer.as_str())
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(para, area);

        // 设置光标位置：输入框内 (1, 1) 起始（边框 1px），加上光标列
        let cursor_x = area.x + 1 + u16::try_from(self.input.cursor_col()).unwrap_or(u16::MAX);
        let cursor_y = area.y + 1;
        frame.set_cursor_position((cursor_x, cursor_y));
    }

    fn render_permission_popup(frame: &mut Frame, area: Rect, pending: &PendingPermission) {
        let width = 60.min(area.width);
        let height = 7;
        let x = area.x + (area.width - width) / 2;
        let y = area.y + (area.height - height) / 2;
        let popup = Rect::new(x, y, width, height);

        let risk_color = match pending.prompt.risk {
            minicoding_core::policy::Risk::Low => Color::Green,
            minicoding_core::policy::Risk::Medium => Color::Yellow,
            minicoding_core::policy::Risk::High => Color::Red,
        };
        let options: Vec<String> = pending
            .prompt
            .options
            .iter()
            .map(|opt| option_label(*opt))
            .collect();
        let text = vec![
            Line::from(vec![
                Span::styled("权限请求 ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("[{:?}]", pending.prompt.risk),
                    Style::default().fg(risk_color),
                ),
            ]),
            Line::from(format!("工具: {}", pending.prompt.tool)),
            Line::from(format!("说明: {}", pending.prompt.summary)),
            Line::raw(""),
            Line::from(format!("选项: {}", options.join(" / "))),
            Line::styled(
                "[y] 允许  [n] 拒绝  [Esc] 拒绝",
                Style::default().fg(Color::Cyan),
            ),
        ];
        let block = Block::default()
            .borders(Borders::ALL)
            .style(Style::default().bg(Color::Black));
        let para = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
        frame.render_widget(para, popup);
    }
}

/// 选项标签。
fn option_label(opt: PromptOption) -> String {
    match opt {
        PromptOption::AllowOnce => "允许一次".to_string(),
        PromptOption::AllowAlways => "始终允许".to_string(),
        PromptOption::DenyOnce => "拒绝一次".to_string(),
        PromptOption::DenyAlways => "始终拒绝".to_string(),
    }
}

/// 从 `Arc<Runtime>` 提取 cancel token 用于中断（主循环 Ctrl-C 调用）。
///
/// 返回 `Arc` 的引用便于 UI 在中断时调用 `cancel()`。
#[allow(dead_code)] // T-M7-1 暂未接入中断按钮，T-M7-3/4 接入
pub fn runtime_cancel_token(
    rt: &Arc<minicoding_core::runtime::Runtime>,
) -> tokio_util::sync::CancellationToken {
    rt.cancel_token()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn input_state_insert_and_backspace() {
        let mut s = InputState::default();
        s.insert_char('你');
        s.insert_char('好');
        assert_eq!(s.buffer, "你好");
        assert_eq!(s.cursor, 6); // 2 个中文字符 = 6 字节
        s.backspace();
        assert_eq!(s.buffer, "你");
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn input_state_cursor_movement() {
        let mut s = InputState::default();
        s.insert_char('a');
        s.insert_char('b');
        s.insert_char('c');
        assert_eq!(s.cursor, 3);
        s.cursor_left();
        assert_eq!(s.cursor, 2);
        s.cursor_left();
        assert_eq!(s.cursor, 1);
        s.cursor_right();
        assert_eq!(s.cursor, 2);
        s.cursor_home();
        assert_eq!(s.cursor, 0);
        s.cursor_end();
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn input_state_submit_records_history() {
        let mut s = InputState::default();
        s.insert_char('h');
        s.insert_char('i');
        let text = s.submit();
        assert_eq!(text.as_deref(), Some("hi"));
        assert!(s.buffer.is_empty(), "expected empty: s.buffer");
        assert_eq!(s.history, vec!["hi".to_string()]);
        // 重复内容不重复记录
        s.insert_char('h');
        s.insert_char('i');
        s.submit();
        assert_eq!(s.history.len(), 1);
    }

    #[test]
    fn input_state_submit_empty_returns_none() {
        let mut s = InputState::default();
        s.insert_char(' ');
        assert!(s.submit().is_none());
    }

    #[test]
    fn input_state_history_navigation() {
        let mut s = InputState {
            buffer: "first".to_string(),
            cursor: 5,
            history: Vec::new(),
            history_idx: None,
        };
        s.submit();
        s.buffer = "second".to_string();
        s.cursor = 6;
        s.submit();
        assert_eq!(s.history.len(), 2);
        // 从空缓冲按上 → 最近一条
        s.history_prev();
        assert_eq!(s.buffer, "second");
        s.history_prev();
        assert_eq!(s.buffer, "first");
        s.history_next();
        assert_eq!(s.buffer, "second");
        s.history_next();
        assert!(s.buffer.is_empty(), "expected empty: s.buffer");
    }

    #[test]
    fn input_state_cursor_col_counts_chars_not_bytes() {
        let mut s = InputState::default();
        s.insert_char('a');
        s.insert_char('中');
        s.insert_char('b');
        // 光标在末尾：3 个字符
        assert_eq!(s.cursor_col(), 3);
        s.cursor_left();
        assert_eq!(s.cursor_col(), 2);
    }
}
