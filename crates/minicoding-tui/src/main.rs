//! # minicoding-tui（bin）
//!
//! TUI frontend 入口（T-M7-1）：非 TTY 降级检测 → 构建 `TuiPrompter` + `Runtime`
//! → 启动 tokio 后台桥接 → ratatui 全屏事件循环。
//!
//! T-M7-2：会话切换由 main 外层循环处理——`run_app` 返回后检查
//! `app.take_pending_switch()`，若有挂起切换则重建 Runtime（`SessionLoadMode::Resume`）
//! 并重新进入主循环。
//!
//! ## 退出码
//!
//! 成功 0；运行时错误 1；非 TTY 降级 2。
//!
//! 详见 `docs/modules.md` §13。

#![deny(clippy::all, clippy::pedantic)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use minicoding_cli::builder::{self, SessionLoadMode};
use minicoding_core::policy::{PermissionPrompter, TuiPermissionRequest};
use minicoding_core::storage::SessionMeta;
use minicoding_policy::TuiPrompter;
use minicoding_storage::JsonlStorage;
use minicoding_tui::{App, AppEvent, UiCommand, spawn_runtime_bridge};
use std::io::IsTerminal;
use tokio::sync::mpsc;

/// TUI 主入口。
///
/// # Errors
/// Runtime 构建失败、终端初始化失败、事件循环 IO 错误时返回错误。
fn main() -> Result<()> {
    // 非 TTY 降级（T-M7-1 验收：非 TTY 降级为 CLI 模式）
    if !std::io::stdout().is_terminal() {
        eprintln!("minicoding-tui 需要 TTY 终端。非交互环境请使用 `minicoding` CLI。");
        std::process::exit(2);
    }

    let workdir = std::env::current_dir()
        .context("无法确定当前目录")?
        .to_string_lossy()
        .into_owned();

    // ratatui 初始化（alternate screen + raw mode），整个会话切换过程保持开启
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &workdir, &SessionLoadMode::None);
    ratatui::restore();
    result
}

/// 外层循环：构建 Runtime → 进入主循环 → 检查会话切换请求 → 必要时重建。
///
/// 会话切换（T-M7-2）通过重建 Runtime 实现：`SessionLoadMode::Resume(new_id)`。
/// UI 状态（输入历史、侧栏选择）在重建间不保留（新 `App` 实例），因为切换
/// 目标是不同会话的历史，旧 UI 状态无意义。
fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    workdir: &str,
    initial_mode: &SessionLoadMode,
) -> Result<()> {
    let mut current_mode = initial_mode.clone();
    loop {
        let (app, mut rt_rx) = build_and_start(workdir, &current_mode)?;
        let (mut app, ()) = run_app(terminal, app, &mut rt_rx)?;

        // 检查会话切换请求（T-M7-2）
        if let Some(new_id) = app.take_pending_switch() {
            current_mode = SessionLoadMode::Resume(new_id);
            continue;
        }
        return Ok(());
    }
}

/// 构建 Runtime + 启动桥接 + 创建 App。
///
/// 返回 `(App, rt_rx)`——`App` 持有 `ui_tx`，`rt_rx` 由主循环消费。
fn build_and_start(
    workdir: &str,
    mode: &SessionLoadMode,
) -> Result<(App, mpsc::Receiver<AppEvent>)> {
    // T-M7-3：TuiPrompter 通过 mpsc channel 把权限询问发给 UI（非阻塞弹窗）。
    let (perm_tx, perm_rx) = mpsc::channel::<TuiPermissionRequest>(8);
    let prompter: Arc<dyn PermissionPrompter> = Arc::new(TuiPrompter::new(perm_tx));

    let rt = builder::build_runtime(
        None,
        None,
        None,
        None,
        workdir,
        None,
        mode,
        None,
        false,
        Some(prompter),
    )
    .context("构建 Runtime 失败")?;

    // 加载会话列表（侧栏显示，T-M7-2）
    let sessions = load_sessions();
    let current_session_id = rt.session().id.clone();
    // 注：resume 模式下，`builder::build_runtime` 已加载历史消息到 `session.messages`，
    // 但未注入 `ContextManager`（需 async `restore_history`）。桥接线程首个
    // `run_turn` 前会通过 `Runtime::restore_history` 回填——见 builder.rs 文档。

    // UI ↔ Runtime channel
    let (ui_tx, ui_rx) = mpsc::channel::<UiCommand>(16);
    let (rt_tx, rt_rx) = mpsc::channel::<AppEvent>(256);
    spawn_runtime_bridge(rt, ui_rx, perm_rx, rt_tx);

    let app = App::new(ui_tx, sessions, current_session_id);
    Ok((app, rt_rx))
}

/// 加载最近会话列表（按 `last_message_at` 倒序，最多 50 条）。
fn load_sessions() -> Vec<SessionMeta> {
    let Ok(sessions_dir) = minicoding_core::paths::sessions_dir() else {
        return Vec::new();
    };
    let storage = JsonlStorage::new(sessions_dir);
    match storage.list_sessions_sync() {
        Ok(mut sessions) => {
            // 按 last_message_at 倒序
            sessions.sort_by_key(|s| std::cmp::Reverse(s.last_message_at));
            sessions.truncate(50);
            sessions
        }
        Err(e) => {
            tracing::warn!("加载会话列表失败：{e}");
            Vec::new()
        }
    }
}

/// TUI 主循环：draw → 非阻塞消费 Runtime 事件 → 轮询终端事件（100ms 窗口）。
///
/// 消费 `app`，返回 `(app, Result)`——调用方检查 `app.take_pending_switch()`。
fn run_app(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    rt_rx: &mut mpsc::Receiver<AppEvent>,
) -> Result<(App, ())> {
    loop {
        terminal.draw(|f| app.render(f))?;

        // 非阻塞消费所有已到达的 Runtime 事件
        while let Ok(ev) = rt_rx.try_recv() {
            app.handle_app_event(ev);
        }

        // 轮询终端事件（100ms 超时，让循环定期回到 draw 刷新 Runtime 事件）
        if crossterm::event::poll(Duration::from_millis(100))? {
            let ev = crossterm::event::read()?;
            app.handle_app_event(AppEvent::Term(ev));
        }

        if app.should_exit() {
            break;
        }
    }
    Ok((app, ()))
}
