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
use minicoding_core::policy::{PermissionPrompter, TuiPermissionRequest};
use minicoding_core::storage::SessionListItem;
use minicoding_policy::TuiPrompter;
use minicoding_sdk::builder::{self, SessionLoadMode};
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

    let _tracing_guard = init_tui_tracing();

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

/// ENG-4（2026-08-26 R3 审查）：安装 tracing subscriber——此前 TUI 入口从未
/// 初始化任何 subscriber，全部 `tracing::*` 事件静默丢弃（观测性四入口缺一）。
/// alternate screen 下 stderr 不可见，日志写 `~/.minicoding/logs/tui.log`
/// （按天轮转，保留 3 份）；home 不可解析时跳过（best-effort，不阻塞启动）。
fn init_tui_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    let home = minicoding_core::paths::minicoding_home().ok()?;
    if std::fs::create_dir_all(home.join("logs")).is_err() {
        return None;
    }
    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("tui.log")
        .max_log_files(3)
        .build(home.join("logs"))
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_writer(writer),
        )
        .with(filter)
        .try_init();
    Some(guard)
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
use minicoding_tui::app::ChatLine;

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
    let cancel_token = rt.cancel_token();
    // 恢复会话历史 → UI 聊天区初始行（§11-P1：此前 restore_history 只回填
    // 上下文，UI 从空白开始）。工具输出行不回放（噪音大），带 tool_calls 的
    // assistant 显示为已完成的工具行。
    let history: Vec<ChatLine> = rt
        .session()
        .messages
        .iter()
        .filter_map(|m| match m.role {
            minicoding_core::model::Role::User => {
                let t = m.text();
                (!t.is_empty()).then_some(ChatLine::User(t))
            }
            minicoding_core::model::Role::Assistant => {
                if m.tool_calls.is_empty() {
                    let t = m.text();
                    (!t.is_empty()).then_some(ChatLine::Assistant(t))
                } else {
                    Some(ChatLine::Tool {
                        tool: m
                            .tool_calls
                            .iter()
                            .map(|c| c.name.clone())
                            .collect::<Vec<_>>()
                            .join(","),
                        done: true,
                    })
                }
            }
            _ => None,
        })
        .collect();
    let (ui_tx, ui_rx) = mpsc::channel::<UiCommand>(16);
    let (rt_tx, rt_rx) = mpsc::channel::<AppEvent>(256);
    spawn_runtime_bridge(rt, ui_rx, perm_rx, rt_tx);

    let mut app = App::new(ui_tx, sessions, current_session_id);
    app.set_cancel_token(cancel_token);
    app.set_history(history);
    Ok((app, rt_rx))
}

/// 加载最近会话列表（按 `last_message_at` 倒序，最多 50 条）。
fn load_sessions() -> Vec<SessionListItem> {
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
