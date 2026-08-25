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
use minicoding_core::util::slash::{self, SlashCommand};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{Context as RlContext, Editor, Helper};
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
#[allow(clippy::too_many_lines)] // 斜杠命令分派表线性展开，拆分降低可读性
pub async fn run_interactive_session(rt: &Runtime) -> i32 {
    let mut rl = match Editor::<AtFileHelper, DefaultHistory>::new() {
        Ok(mut rl) => {
            rl.set_helper(Some(AtFileHelper::default()));
            rl
        }
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

    // 会话累计 token（遗留#7）：run_one_turn 返回 assistant metadata.tokens
    let mut session_tokens: usize = 0;
    let mut turn_count: usize = 0;
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

        // 斜杠命令分派（T-M5-8；F3：解析下沉 `core::util::slash` 共享，
        // TUI/CLI 单一事实来源）。`/quit`、`/exit`（退出语义）与 `/plan`
        // 的 `on|off|status` 子命令不在共享 parser 语义内，保留 CLI 原生处理。
        if let Some(cmd) = trimmed.strip_prefix('/') {
            let name = cmd.split_whitespace().next().unwrap_or("");
            if matches!(name, "quit" | "exit") {
                break;
            }
            if name == "plan" {
                handle_plan_command(rt, cmd.split_whitespace().nth(1)).await;
                continue;
            }
            match slash::parse(trimmed) {
                Some(SlashCommand::Help) => print_help(),
                Some(SlashCommand::Model(Some(m))) => {
                    rt.set_model(&m);
                    anstream::eprintln!("{GREEN}模型已切换（会话级）：{m}{GREEN:#}");
                }
                Some(SlashCommand::Model(None)) => {
                    anstream::eprintln!("{DIM}当前模型：{}{DIM:#}", rt.model());
                }
                Some(SlashCommand::Status) => {
                    handle_status_command(rt, session_tokens, turn_count).await;
                }
                Some(SlashCommand::Tokens) => print_tokens(session_tokens, turn_count),
                Some(SlashCommand::Clear) => {
                    // 软清屏：仅清终端显示，会话上下文保留（清上下文需
                    // ContextManager 截断 API，暂未提供）
                    anstream::eprint!("\x1b[2J\x1b[H");
                    anstream::eprintln!("{DIM}已清屏（会话上下文保留）{DIM:#}");
                }
                Some(SlashCommand::Undo { steps }) => handle_undo_command(rt, steps).await,
                // 行为保持：裸 "/" 此前静默跳过
                Some(SlashCommand::Unknown(name)) if name.is_empty() => {}
                Some(SlashCommand::Unknown(name)) => {
                    anstream::eprintln!("{RED}未知命令: /{name}（/help 查看可用命令）{RED:#}");
                }
                // `/plan` 已在上方原生处理，parser 的 PlanToggle 分支不可达；
                // 显式展开以应对 parser 未来扩展
                Some(SlashCommand::PlanToggle) => handle_plan_command(rt, None).await,
                None => {}
            }
            continue;
        }

        let _ = rl.add_history_entry(&line);
        // @文件引用注入（遗留：对标 CC @file）：正文保留原 token 供模型理解
        // 指代，文件内容以 <file_ref> 边界附于消息尾部
        let expanded = expand_at_refs(&line, &rt.workdir().await);
        let used = run_one_turn(rt, expanded).await;
        session_tokens += used;
        turn_count += 1;
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
/// 调用 `Journal::undo(steps)` 回滚最近 `steps` 次文件改动 operation（共享
/// parser 保证 `steps ≥ 1`）。`file-undo` feature 未启用或 journal 未注入时
/// 打印提示。回滚结果（成功/冲突）打印到 stderr。
async fn handle_undo_command(rt: &Runtime, steps: usize) {
    let Some(journal) = rt.journal() else {
        anstream::eprintln!(
            "{YELLOW}/undo 不可用：未启用 file-undo feature（重新编译时加 --features file-undo）{YELLOW:#}"
        );
        return;
    };
    let span = tracing::info_span!("undo", session = %rt.session().id, otel.name = "undo");
    let _enter = span.enter();
    match journal.undo(steps).await {
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
            // S28/C-28：/undo 反向恢复也落审计（成功含恢复与冲突清单摘要）
            let detail = format!(
                "undone_entries={} restored={} conflicts={}",
                report.undone_entries,
                report.restored_files.len(),
                report.failed_files.len()
            );
            if let Err(e) = rt
                .audit()
                .record(minicoding_core::storage::AuditRecord {
                    ts: time::OffsetDateTime::now_utc(),
                    session: rt.session().id.clone(),
                    kind: minicoding_core::storage::AuditKind::FileUndone,
                    tool: Some("undo".to_string()),
                    decision: Some("allow".to_string()),
                    detail,
                })
                .await
            {
                tracing::warn!(error = %e, "undo audit record failed");
            }
        }
        Err(e) => {
            anstream::eprintln!("{RED}/undo 失败：{e}{RED:#}");
            // S28：失败的回滚同样留痕
            if let Err(e2) = rt
                .audit()
                .record(minicoding_core::storage::AuditRecord {
                    ts: time::OffsetDateTime::now_utc(),
                    session: rt.session().id.clone(),
                    kind: minicoding_core::storage::AuditKind::FileUndone,
                    tool: Some("undo".to_string()),
                    decision: Some("deny".to_string()),
                    detail: format!("failed: {e}"),
                })
                .await
            {
                tracing::warn!(error = %e2, "undo audit record failed");
            }
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
    anstream::eprintln!("{DIM}  /model [name]      查看/切换模型（会话级生效）{DIM:#}");
    anstream::eprintln!("{DIM}  /status            会话状态摘要{DIM:#}");
    anstream::eprintln!("{DIM}  /tokens            本会话 token 计量{DIM:#}");
    anstream::eprintln!("{DIM}  /clear             清屏（上下文保留）{DIM:#}");
    anstream::eprintln!("{DIM}Ctrl-C：提示符处连续两次退出；turn 运行时取消当前回合{DIM:#}");
    anstream::eprintln!("{DIM}其他输入作为提问发送给助手。{DIM:#}");
}

/// 运行单轮对话，附带事件渲染与 Ctrl-C 取消。
async fn run_one_turn(rt: &Runtime, line: String) -> usize {
    // 返回值：本 turn 消耗 token（assistant metadata.tokens）
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
                // FE-8（2026-08-25 R2 审查）：reasoning 增量以暗色渲染——此前
                // CLI 丢弃 ReasoningDelta，思考过程仅 Web/SDK 可见（四形态
                // 能力矩阵漂移）。
                Ok(Event::ReasoningDelta(text)) => {
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    let _ = write!(lock, "{DIM}{text}{DIM:#}");
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
        Ok(TurnOutcome::Finished(msg) | TurnOutcome::Interrupted(msg)) => {
            if !msg.text().is_empty() {
                println!();
            }
            // token 计量（遗留#7）：metadata.tokens 为 provider Usage.output_tokens
            let used = msg.metadata.tokens.unwrap_or(0);
            if used > 0 {
                anstream::eprintln!("{DIM}tokens(+{used}){DIM:#}");
            }
            used
        }
        Ok(TurnOutcome::Failed(e)) => {
            println!();
            anstream::eprintln!("{RED}错误: {e}{RED:#}");
            0
        }
        Err(e) => {
            println!();
            anstream::eprintln!("{RED}运行时错误: {e}{RED:#}");
            0
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

/// `/status`：会话状态摘要（遗留#7）。
async fn handle_status_command(rt: &Runtime, session_tokens: usize, turn_count: usize) {
    let snap = rt.plan_controller().snapshot().await;
    let ctx_tokens = rt.context().token_count();
    let model = rt.model();
    anstream::eprintln!("{DIM}── 会话状态 ──{DIM:#}");
    anstream::eprintln!("{DIM}session : {}{DIM:#}", rt.session().id);
    anstream::eprintln!("{DIM}model   : {model}{DIM:#}");
    anstream::eprintln!("{DIM}mode    : {:?}{DIM:#}", snap.mode);
    anstream::eprintln!("{DIM}turns   : {turn_count}{DIM:#}");
    anstream::eprintln!("{DIM}ctx     : ~{ctx_tokens} tokens{DIM:#}");
    print_tokens(session_tokens, turn_count);
}

/// `/tokens`：会话级 token 计量输出。
fn print_tokens(session_tokens: usize, turn_count: usize) {
    if turn_count == 0 {
        anstream::eprintln!("{DIM}tokens: 尚无用量{DIM:#}");
        return;
    }
    anstream::eprintln!(
        "{DIM}tokens: 会话累计 {session_tokens}（均 {}/turn，共 {turn_count} turn）{DIM:#}",
        session_tokens / turn_count
    );
}

// ==================== @文件引用（遗留：对标 CC @path）====================

/// `@路径` 自动补全 Helper（Tab 触发）：补全光标前最后一个未闭合的
/// `@` token 之后的相对路径（工作目录内，含目录并追加 `/`）。
#[derive(Default)]
struct AtFileHelper {
    _priv: (),
}

impl Completer for AtFileHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &RlContext<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        let Some(at) = before.rfind('@') else {
            return Ok((pos, Vec::new()));
        };
        // '@' 之后到光标之间不能有空白（否则不是进行中的路径 token）
        let prefix = &before[at + 1..];
        if prefix.contains(char::is_whitespace) {
            return Ok((pos, Vec::new()));
        }
        let workdir = std::env::current_dir().unwrap_or_default();

        // prefix 拆 (目录部分, 文件名前缀)
        let (dir_rel, name_prefix) = match prefix.rfind('/') {
            Some(i) => (&prefix[..=i], &prefix[i + 1..]),
            None => ("", prefix),
        };
        let scan_dir = if dir_rel.is_empty() {
            workdir.clone()
        } else {
            workdir.join(dir_rel)
        };
        let mut candidates = Vec::new();
        let Ok(entries) = std::fs::read_dir(&scan_dir) else {
            return Ok((pos, Vec::new()));
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(name_prefix) || name.starts_with('.') {
                continue;
            }
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            candidates.push(Pair {
                display: format!("{}{}", name, if is_dir { "/" } else { "" }),
                replacement: format!("{dir_rel}{name}{}", if is_dir { "/" } else { "" }),
            });
        }
        candidates.sort_by(|a, b| a.replacement.cmp(&b.replacement));
        Ok((at + 1, candidates))
    }
}

// rustyline 需要空实现的其他 trait
impl rustyline::hint::Hinter for AtFileHelper {
    type Hint = String;
}
impl rustyline::highlight::Highlighter for AtFileHelper {}
impl rustyline::validate::Validator for AtFileHelper {}
impl Helper for AtFileHelper {}

/// 展开用户输入中的 `@相对路径` 引用（遗留：对标 CC @file）。
///
/// 每个 `@path` token（至空白结束；支持引号包裹 `"@a b.txt"`）在 workdir 内
/// 经 `resolve_under` 校验后读取内容，以 `<file_ref path>` 边界附加到消息尾部
/// （单文件 32KiB 截断）；越界/读取失败替换为错误占位提示，不静默丢弃。
/// 原始 token 从正文移除，保持提问简洁。
#[must_use]
fn expand_at_refs(line: &str, workdir: &camino::Utf8PathBuf) -> String {
    use minicoding_policy::resolve_under;

    const MAX_FILE_BYTES: usize = 32 * 1024;
    use std::fmt::Write as _;
    let mut body = line.to_string();
    let mut attachments = String::new();

    // 简易 tokenizer：优先匹配引号包裹的 "@..."，再匹配裸 @非空白+
    let mut out = String::new();
    let bytes: Vec<char> = body.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == '@' && (i == 0 || bytes[i - 1].is_whitespace()) {
            // 收集 token 至空白
            let start = i;
            let mut j = i + 1;
            while j < bytes.len() && !bytes[j].is_whitespace() {
                j += 1;
            }
            let path_ref: String = bytes[start + 1..j].iter().collect();
            i = j;
            if path_ref.is_empty() {
                out.push('@');
                continue;
            }
            match resolve_under(workdir, &path_ref) {
                Ok(abs) => match std::fs::read(abs.as_std_path()) {
                    Ok(bytes_raw) => {
                        let content = if bytes_raw.len() > MAX_FILE_BYTES {
                            let cut = &bytes_raw[..MAX_FILE_BYTES];
                            format!(
                                "{}\n…[截断，共 {} 字节]",
                                String::from_utf8_lossy(cut),
                                bytes_raw.len()
                            )
                        } else {
                            String::from_utf8_lossy(&bytes_raw).into_owned()
                        };
                        let _ = write!(
                            attachments,
                            "\n<file_ref path=\"{path_ref}\">\n{content}\n</file_ref>"
                        );
                    }
                    Err(e) => {
                        let _ = write!(
                            attachments,
                            "\n<file_ref path=\"{path_ref}\" error=\"读取失败: {e}\" />"
                        );
                    }
                },
                Err(e) => {
                    let _ = write!(
                        attachments,
                        "\n<file_ref path=\"{path_ref}\" error=\"路径越界或不存在: {e}\" />"
                    );
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    drop(bytes);
    body = out.trim_end().to_string();
    if attachments.is_empty() {
        body
    } else {
        format!("{body}\n{attachments}")
    }
}
