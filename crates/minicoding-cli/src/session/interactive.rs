//! 交互 REPL 会话模式（`--session`，T-M2-8）。
//!
//! 基于 `rustyline` 的行编辑循环：`read_line` → `run_turn` → 渲染 → 循环。
//!
//! - 斜杠命令：`/quit`/`/exit` 退出、`/help` 帮助、`/plan` 切换 Plan 模式
//!   （T-M5-8）、`/undo` 回滚最近一次文件改动 operation（T-M5-8，`file-undo`
//!   feature）；空行跳过。
//! - Ctrl-C：在提示符处连续两次退出；在 turn 运行时取消当前回合（graceful stop，
//!   C-13：已落盘消息不丢失）。
//! - Ctrl-D（EOF）：退出。
//!
//! 终端模式说明：readline 期间终端处于 raw 模式，Ctrl-C 作为字节 0x03 被
//! rustyline 捕获为 `Interrupted`（不产生 SIGINT）；turn 运行期间终端回到
//! cooked 模式，Ctrl-C 产生 SIGINT 由 `tokio::signal::ctrl_c` 捕获并调用
//! `rt.cancel()`。两条路径互不干扰。
//!
//! 事件渲染订阅 `EventBus`：token 实时写 stdout（复用单次模式逻辑），工具调用 /
//! 权限请求 / 失败摘要写 stderr，保持 stdout 干净以承载 LLM 回复。

use std::io::Write;
use std::time::Duration;

use anstyle::{AnsiColor, Color, Style};
use minicoding_core::model::{ToolContent, TurnOutcome, UserInput};
use minicoding_core::policy::PermissionMode;
use minicoding_core::runtime::{Event, Runtime};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::sync::broadcast::error::RecvError;

/// REPL 提示符。
const PROMPT: &str = "minicoding> ";

/// 渲染任务等待 `TurnEnd` 的超时。
///
/// `run_turn` 在 `Finished`/`Interrupted`/超时路径均 emit `TurnEnd`，但 `Failed`
/// 路径不 emit；用超时兜底防止渲染任务挂死。
const RENDER_FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

/// 工具结果预览的最大字符数。
const PREVIEW_MAX: usize = 80;

/// dim 文本样式（工具调用 / 状态行）。
const DIM: Style = Style::new().dimmed();
/// 红色样式（失败）。
const RED: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)));
/// 绿色样式（成功）。
const GREEN: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
/// 黄色样式（Plan 模式 / 提醒）。
const YELLOW: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));

/// 运行交互 REPL 会话。
///
/// 返回退出码：0 正常退出，1 初始化 / IO 致命错误。
pub async fn run_interactive_session(rt: &Runtime) -> i32 {
    let mut rl = match DefaultEditor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("初始化行编辑器失败: {e}");
            return 1;
        }
    };

    // 启动时若处于 Plan 模式（`--plan`），打印提示。
    let snap = rt.plan_controller().snapshot().await;
    if snap.mode == PermissionMode::Plan {
        anstream::eprintln!(
            "{YELLOW}Plan 模式已激活：副作用工具被硬门拒绝，调 plan.exit 后进入执行阶段{YELLOW:#}"
        );
    }

    anstream::eprintln!("{DIM}minicoding 交互会话（/help 查看命令，/quit 或 Ctrl-D 退出）{DIM:#}");

    let mut consecutive_ctrlc: u8 = 0;

    loop {
        let line = match rl.readline(PROMPT) {
            Ok(line) => line,
            Err(ReadlineError::Eof) => break,
            Err(ReadlineError::Interrupted) => {
                // 提示符处 Ctrl-C：连续两次退出，否则继续
                consecutive_ctrlc = consecutive_ctrlc.saturating_add(1);
                if consecutive_ctrlc >= 2 {
                    anstream::eprintln!("{DIM}（再次 Ctrl-C，退出）{DIM:#}");
                    break;
                }
                anstream::eprintln!("{DIM}（Ctrl-C 取消当前输入；再按一次退出）{DIM:#}");
                continue;
            }
            Err(e) => {
                eprintln!("读取输入失败: {e}");
                break;
            }
        };

        consecutive_ctrlc = 0;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // 斜杠命令分派（T-M5-8：新增 /plan、/undo）
        if let Some(cmd) = trimmed.strip_prefix('/') {
            let mut parts = cmd.split_whitespace();
            let name = parts.next().unwrap_or("");
            match name {
                "quit" | "exit" => break,
                "help" => {
                    print_help();
                    continue;
                }
                "plan" => {
                    handle_plan_command(rt, parts.next()).await;
                    continue;
                }
                "undo" => {
                    handle_undo_command(rt).await;
                    continue;
                }
                "" => {
                    continue;
                }
                _ => {
                    anstream::eprintln!("{RED}未知命令: /{name}（/help 查看可用命令）{RED:#}");
                    continue;
                }
            }
        }

        let _ = rl.add_history_entry(&line);
        run_one_turn(rt, line).await;
    }

    anstream::eprintln!("{DIM}再见{DIM:#}");
    0
}

/// 处理 `/plan` 命令（T-M5-8）。
///
/// - `/plan` 或 `/plan on`：切换到 Plan 模式（只读强制）；
/// - `/plan off`：切换回 Default 模式；
/// - `/plan status`：查询当前模式。
///
/// 切换走 `PlanModeController::set_mode`（与 `plan.exit` 工具的 `exit_plan` 不同：
/// `set_mode` 不校验当前模式，CLI 显式切换；不重置 `allowed_prompts` 缓存）。
async fn handle_plan_command(rt: &Runtime, sub: Option<&str>) {
    let controller = rt.plan_controller();
    let snap = controller.snapshot().await;
    match sub {
        None | Some("on") => {
            if snap.mode == PermissionMode::Plan {
                anstream::eprintln!("{DIM}已在 Plan 模式{DIM:#}");
                return;
            }
            controller.set_mode(PermissionMode::Plan).await;
            anstream::eprintln!("{YELLOW}已切换到 Plan 模式：副作用工具被硬门拒绝{YELLOW:#}");
        }
        Some("off") => {
            if snap.mode == PermissionMode::Default {
                anstream::eprintln!("{DIM}已处于 Default 模式{DIM:#}");
                return;
            }
            controller.set_mode(PermissionMode::Default).await;
            anstream::eprintln!("{GREEN}已切换到 Default 模式{GREEN:#}");
        }
        Some("status") => {
            anstream::eprintln!("{DIM}当前权限模式：{:?}{DIM:#}", snap.mode);
        }
        Some(other) => {
            anstream::eprintln!(
                "{RED}未知子命令: /plan {other}（用法：/plan [on|off|status]）{RED:#}"
            );
        }
    }
}

/// 处理 `/undo` 命令（T-M5-8）。
///
/// 调用 `Journal::undo(1)` 回滚最近一次文件改动 operation。`file-undo` feature
/// 未启用或 journal 未注入时打印提示。回滚结果（成功/冲突）打印到 stderr。
async fn handle_undo_command(rt: &Runtime) {
    let Some(journal) = rt.journal() else {
        anstream::eprintln!(
            "{YELLOW}/undo 不可用：未启用 file-undo feature（重新编译时加 --features file-undo）{YELLOW:#}"
        );
        return;
    };
    let span = tracing::info_span!("undo", session = %rt.session().id, otel.name = "undo");
    let _enter = span.enter();
    match journal.undo(1).await {
        Ok(report) => {
            anstream::eprintln!(
                "{GREEN}已回滚 {} 个 operation（{} 文件恢复，{} 文件冲突）{GREEN:#}",
                report.undone_entries,
                report.restored_files.len(),
                report.failed_files.len(),
            );
            for path in &report.restored_files {
                anstream::eprintln!("{DIM}  恢复：{path}{DIM:#}");
            }
            for (path, reason) in &report.failed_files {
                anstream::eprintln!("{RED}  冲突：{path}（{reason}）{RED:#}");
            }
        }
        Err(e) => {
            anstream::eprintln!("{RED}/undo 失败：{e}{RED:#}");
        }
    }
}

/// 打印 REPL 帮助。
fn print_help() {
    anstream::eprintln!("{DIM}可用命令：{DIM:#}");
    anstream::eprintln!("{DIM}  /help              显示此帮助{DIM:#}");
    anstream::eprintln!("{DIM}  /quit              退出会话（同 /exit、Ctrl-D）{DIM:#}");
    anstream::eprintln!("{DIM}  /exit              退出会话{DIM:#}");
    anstream::eprintln!("{DIM}  /plan [on|off|status]  切换 Plan 模式（只读强制，T-M5-8）{DIM:#}");
    anstream::eprintln!(
        "{DIM}  /undo              回滚最近一次文件改动 operation（T-M5-8）{DIM:#}"
    );
    anstream::eprintln!("{DIM}Ctrl-C：提示符处连续两次退出；turn 运行时取消当前回合{DIM:#}");
    anstream::eprintln!("{DIM}其他输入作为提问发送给助手。{DIM:#}");
}

/// 运行单轮对话，附带事件渲染与 Ctrl-C 取消。
async fn run_one_turn(rt: &Runtime, line: String) {
    let mut rx = rt.events().subscribe();

    // 渲染任务：消费事件流直到 `TurnEnd` 或通道关闭。
    // stdout 写 token（增量 flush），stderr 写工具 / 权限摘要。
    let render_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(Event::Token(text)) => {
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    let _ = write!(lock, "{text}");
                    let _ = lock.flush();
                }
                Ok(Event::ToolCallStarted { tool, .. }) => {
                    anstream::eprintln!();
                    anstream::eprintln!("{DIM}[工具调用: {tool}]{DIM:#}");
                }
                Ok(Event::ToolCallFinished { result, .. }) => {
                    let preview = summarize_content(&result.content);
                    if result.is_error {
                        anstream::eprintln!("{RED}[失败: {preview}]{RED:#}");
                    } else {
                        anstream::eprintln!("{GREEN}[完成: {preview}]{GREEN:#}");
                    }
                }
                Ok(Event::PermissionRequested { summary, .. }) => {
                    anstream::eprintln!("{DIM}[权限请求] {summary}{DIM:#}");
                }
                Ok(Event::PermissionModeChanged { from, to }) => {
                    anstream::eprintln!("{YELLOW}[权限模式切换] {from:?} → {to:?}{YELLOW:#}");
                }
                Ok(Event::TurnEnd { .. }) | Err(RecvError::Closed) => break,
                Ok(_) | Err(RecvError::Lagged(_)) => {}
            }
        }
    });

    // Ctrl-C 处理：turn 运行时（cooked 模式）SIGINT → 取消当前回合。
    // C-13：已落盘消息不丢失，run_turn 返回 Interrupted。
    let cancel_token = rt.cancel_token();
    let ctrl_c_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_token.cancel();
        }
    });

    let user_input = UserInput::from_text(line);
    let result = rt.run_turn(user_input).await;

    ctrl_c_task.abort();

    // 等待渲染任务刷完：正常路径由 TurnEnd 退出；Failed 路径不 emit TurnEnd，靠超时兜底。
    let _ = tokio::time::timeout(RENDER_FLUSH_TIMEOUT, render_task).await;

    match result {
        Ok(TurnOutcome::Finished(msg)) => {
            if !msg.text().is_empty() {
                println!();
            }
        }
        Ok(TurnOutcome::Interrupted(_)) => {
            println!();
            anstream::eprintln!("{DIM}[已取消]{DIM:#}");
        }
        Ok(TurnOutcome::Failed(e)) => {
            println!();
            anstream::eprintln!("{RED}错误: {e}{RED:#}");
        }
        Err(e) => {
            println!();
            anstream::eprintln!("{RED}运行时错误: {e}{RED:#}");
        }
    }
}

/// 将 `ToolContent` 压缩为单行预览（折叠换行、截断过长内容）。
fn summarize_content(content: &ToolContent) -> String {
    let raw = match content {
        ToolContent::Text(s) => s.clone(),
        ToolContent::Json(v) => v.to_string(),
        ToolContent::Image { mime, .. } => return format!("<image/{mime}>"),
        ToolContent::Mixed(parts) => parts
            .iter()
            .map(summarize_content)
            .collect::<Vec<_>>()
            .join(" | "),
    };
    let one_line: String = raw.lines().map(str::trim).collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= PREVIEW_MAX {
        one_line
    } else {
        let truncated: String = one_line.chars().take(PREVIEW_MAX).collect();
        format!("{truncated}…")
    }
}
