//! NDJSON stdio 适配器（T-M8-4）：编辑器插件通过 stdin/stdout NDJSON 协议驱动 minicoding。
//!
//! 协议规范：
//! - **stdin**：每行一个 `Command` JSON（`minicoding_protocol::Command`，tag = `command`）；
//! - **stdout**：每行一个 `EventDto` JSON（`minicoding_protocol::EventDto`，含 `seq` 与 `type`）。
//!
//! ## 命令映射
//!
//! | Command | 响应事件 |
//! |---------|---------|
//! | `CreateSession` | `SessionCreated`（`seq=0`） |
//! | `SendUserMessage` | 流式事件（`Token`/`ToolCallStarted`/`ToolCallFinished`/...）结尾 `TurnEnd` |
//! | `Cancel` | `TurnEnd`（`stop_reason=interrupted`，由 Runtime 自动发） |
//! | `ListSessions` | `SessionsListed`（`seq=0`） |
//! | `GetSession` | `SessionRetrieved`（`seq=0`） |
//! | `SetPermissionMode` | `PermissionModeChanged`（由 Runtime 自动发） |
//! | `ResolvePermission` | `PermissionResolved`（由 Runtime 自动发） |
//! | `Undo` | `UndoReported`（FE-4：与 HTTP `/undo` 对齐；journal 未注入时报错） |
//!
//! ## seq 语义
//!
//! - 流式事件（`SendUserMessage` 期间）：`seq` 来自会话级 cursor（`subscribe_sequenced`），
//!   与 SSE/ACP/LSP 共享同一 seq 空间——跨协议切换/重连游标一致（FE-9，2026-08-28 R5）；
//! - 非流式响应（`SessionCreated`/`SessionsListed`/...）：`seq=0`（非流式事件，不参与 cursor）；
//! - `CommandError`：`seq=0`。
//!
//! ## 权限交互
//!
//! 副作用工具触发时，Runtime 通过 `EventBus` 发 `PermissionRequested` 事件（经
//! `ServerPrompter` 注册 pending 后），NDJSON 适配器转发到 stdout。客户端解析后
//! 通过 `ResolvePermission` 命令回传决策，适配器查找所有会话的 pending 表（NDJSON
//! 单会话场景下通常只有一个）并解析。
//!
//! ## 安全约束
//!
//! - **C-04**：不泄露凭证——`SessionConfig` 不含 `api_key` 字段，server 用 `default_params`；
//! - **C-05**：输出是数据非指令——`EventDto` 是结构化事件，非 LLM 直接输出；
//! - **C-01**：副作用工具调用经 `ServerPrompter` → 客户端决策 → `ResolvePermission`。
//!
//! 详见 `docs/dev-plan.md` T-M8-4、`docs/design.md` §24。

use crate::runtime_builder::ServerRuntimeParams;
use crate::session_mgr::{SessionManager, SessionManagerError};
use minicoding_core::model::TurnOutcome;
use minicoding_protocol::command::Command;
use minicoding_protocol::event::{EventDto, EventKind, NdjsonCommandKind};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// NDJSON 适配器错误。
#[derive(Debug, thiserror::Error)]
pub enum NdjsonError {
    /// stdin/stdout IO 错误。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON 序列化/反序列化错误。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// `SessionManager` 错误（会话不存在、构造失败等）。
    #[error("session manager error: {0}")]
    Session(#[from] SessionManagerError),
    /// 会话不存在（`SendUserMessage`/`Cancel`/`GetSession` 等命令引用了未创建的 `session_id`）。
    #[error("session {0} not found")]
    SessionNotFound(String),
}

/// 共享 stdout writer（`Mutex` 保护，避免 event forwarder 与 main loop 交叉写）。
///
/// 使用 `Box<dyn AsyncWrite + Send + Unpin>` 让生产代码（`Stdout`）与测试代码
/// （`tokio::io::sink()`）共用同一类型，避免为测试单独抽象。
/// 单行 NDJSON 上限（FE-8，2026-08-28 R5 收尾）：恶意/异常客户端可发无限长
/// 单行使 server OOM。256 KiB 覆盖真实请求（含大工具调用参数）。
const MAX_LINE_BYTES: usize = 256 * 1024;
type SharedStdout = Arc<Mutex<tokio::io::BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>;

/// 启动 NDJSON server：从 stdin 读 `Command`，向 stdout 写 `EventDto`。
///
/// 阻塞当前 task，直到 stdin 关闭（EOF）或发生不可恢复错误。
///
/// # Errors
/// - stdin 读取 IO 错误；
/// - stdout 写入 IO 错误；
/// - JSON 解析/序列化错误（理论上不可达，因 `Command`/`EventDto` 都实现了 `Serialize`/`Deserialize`）。
///
/// # 设计要点
///
/// - **不阻塞于单个 turn**：`SendUserMessage` 在后台 task 中执行 `run_turn`，主循环
///   继续读 stdin——客户端可在 turn 进行中发送 `Cancel` 或 `ResolvePermission`；
/// - **多会话**：`SessionManager` 支持多会话，但 NDJSON 客户端通常单会话；
/// - **事件 seq**：`SendUserMessage` 期间事件 seq 单调递增；其它命令响应 `seq=0`。
pub async fn serve_ndjson(mgr: Arc<SessionManager>) -> Result<(), NdjsonError> {
    let stdout: Box<dyn AsyncWrite + Send + Unpin> = Box::new(tokio::io::stdout());
    let stdout: SharedStdout = Arc::new(Mutex::new(tokio::io::BufWriter::new(stdout)));

    // 读 stdin 的 task 与命令消费循环分离：`SendUserMessage` 在 turn 期间阻塞，
    // 但仍须消费后续 `ResolvePermission`/`Cancel` 命令，否则权限交互死锁
    // （turn 等待决策而决策命令躺在 stdin 缓冲区无人读）。
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<Command>(16);
    let reader_stdout = stdout.clone();
    let reader_task = tokio::spawn(async move {
        // FE-8（2026-08-28 R5 收尾）：NDJSON 行读取无上限——恶意/异常本地客户端
        // 可发无限长单行使 server 无限缓冲 OOM。用 `take(MAX_LINE_BYTES + 1)`
        // 截断读：单行超限即报 FrameTooLarge（fail-closed，不继续解析），与
        // ACP 的 Content-Length 上限同一防线。
        let mut reader = BufReader::new(tokio::io::stdin());
        loop {
            let mut line = String::new();
            let n = match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "ndjson: stdin read failed");
                    break;
                }
            };
            // 超限行：丢弃该行（含残余）并报错
            if n > MAX_LINE_BYTES {
                write_command(
                    &reader_stdout,
                    0,
                    &NdjsonCommandKind::CommandError {
                        message: format!("ndjson line exceeds {MAX_LINE_BYTES} bytes"),
                    },
                )
                .await?;
                continue;
            }
            // 空行跳过（编辑器可能发送心跳/空行）
            if line.trim().is_empty() {
                continue;
            }

            // 解析 Command
            let cmd: Command = match serde_json::from_str(&line) {
                Ok(c) => c,
                Err(e) => {
                    write_command(
                        &reader_stdout,
                        0,
                        &NdjsonCommandKind::CommandError {
                            message: format!("invalid command JSON: {e}"),
                        },
                    )
                    .await?;
                    continue;
                }
            };
            if cmd_tx.send(cmd).await.is_err() {
                // 消费端已退出（stdout EOF 等），终止 reader
                break;
            }
        }
        Ok::<(), NdjsonError>(())
    });

    // 消费循环：`SendUserMessage` 阻塞于 turn，其余命令顺序处理
    while let Some(cmd) = cmd_rx.recv().await {
        if let Err(e) = dispatch_command(mgr.clone(), &stdout, &mut cmd_rx, cmd).await {
            tracing::warn!(error = %e, "NDJSON command dispatch failed");
            write_command(
                &stdout,
                0,
                &NdjsonCommandKind::CommandError {
                    message: e.to_string(),
                },
            )
            .await?;
        }
    }

    // stdin EOF：reader task 应已结束；join 回收（不传播错误，进程将退出）
    let _ = reader_task.await;
    Ok(())
}

/// 写一个 `EventDto` 到 stdout（JSON + 换行 + flush）。
async fn write_command(
    stdout: &SharedStdout,
    seq: u64,
    kind: &minicoding_protocol::event::NdjsonCommandKind,
) -> Result<(), NdjsonError> {
    // P3：命令响应走独立枚举，wire 形态与 EventDto 一致（seq + flatten kind）
    #[derive(serde::Serialize)]
    struct Envelope<'a> {
        seq: u64,
        #[serde(flatten)]
        kind: &'a minicoding_protocol::event::NdjsonCommandKind,
    }
    let json = serde_json::to_string(&Envelope { seq, kind })?;
    let mut out = stdout.lock().await;
    out.write_all(json.as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

async fn write_event(stdout: &SharedStdout, seq: u64, kind: &EventKind) -> Result<(), NdjsonError> {
    let dto = EventDto {
        seq,
        kind: kind.clone(),
    };
    let json = serde_json::to_string(&dto)?;
    let mut out = stdout.lock().await;
    out.write_all(json.as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

/// 命令名（用于 turn 进行中拒绝命令时的错误提示）。
fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::CreateSession { .. } => "create_session",
        Command::SendUserMessage { .. } => "send_user_message",
        Command::Cancel { .. } => "cancel",
        Command::Undo { .. } => "undo",
        Command::ListSessions => "list_sessions",
        Command::GetSession { .. } => "get_session",
        Command::SetPermissionMode { .. } => "set_permission_mode",
        Command::ResolvePermission { .. } => "resolve_permission",
    }
}

/// turn 期间收到的命令处理：`ResolvePermission` 唤醒权限等待、`Cancel` 中断
/// turn、其余命令报错（turn 进行中不可用）。
async fn handle_turn_command(
    mgr: Arc<SessionManager>,
    stdout: &SharedStdout,
    cmd: Command,
) -> Result<(), NdjsonError> {
    match cmd {
        Command::ResolvePermission { id, decision } => {
            // NDJSON 通常单会话，遍历所有会话查找 pending permission
            let mut resolved = false;
            for meta in mgr.list_sessions() {
                if mgr
                    .resolve_permission(&meta.id, &id, decision.clone())
                    .await
                    .is_ok()
                {
                    resolved = true;
                    break;
                }
            }
            if !resolved {
                write_command(
                    stdout,
                    0,
                    &NdjsonCommandKind::CommandError {
                        message: format!("permission {id} not found in any session"),
                    },
                )
                .await?;
            }
        }
        Command::Cancel { session_id: sid } => {
            // Cancel 可能针对任意会话（主循环语义），turn 期间同样放行
            if let Err(e) = mgr.cancel(&sid).await {
                tracing::warn!(session_id = %sid, error = %e, "NDJSON cancel failed");
            }
        }
        other => {
            write_command(
                stdout,
                0,
                &NdjsonCommandKind::CommandError {
                    message: format!(
                        "turn in progress, command not accepted: {}",
                        command_name(&other)
                    ),
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// 分派 `Command` 到对应的 handler。`SendUserMessage` 需要 `cmd_rx`：turn 期间
/// 继续消费命令（`ResolvePermission`/`Cancel`），避免权限交互死锁。
#[allow(clippy::too_many_lines)] // 命令分派线性展开（Undo 完整错误映射），拆分支反而切断上下文
async fn dispatch_command(
    mgr: Arc<SessionManager>,
    stdout: &SharedStdout,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<Command>,
    cmd: Command,
) -> Result<(), NdjsonError> {
    match cmd {
        Command::CreateSession { config } => {
            // FE-12/13（2026-08-28 R5 收尾）：与 HTTP 路径对齐——CreateSession
            // 预校验 workdir 存在 + 规范化（canonicalize），否则相对路径隐式绑定
            // server CWD、目录不存在首个 turn 才报错。
            let mut params = build_params_from_config(mgr.default_params(), config);
            if !params.workdir.as_str().trim().is_empty() {
                let canonical =
                    std::fs::canonicalize(params.workdir.as_std_path()).map_err(|e| {
                        NdjsonError::Session(SessionManagerError::BuildFailed(format!(
                            "工作目录不存在或不可访问：{}（{e}）",
                            params.workdir
                        )))
                    })?;
                params.workdir = camino::Utf8PathBuf::from_path_buf(canonical).map_err(|_| {
                    NdjsonError::Session(SessionManagerError::BuildFailed(format!(
                        "工作目录非 UTF-8：{}",
                        params.workdir
                    )))
                })?;
            }
            let session = mgr.create_session(Some(params))?;
            let session_id = session.session_id().clone();
            write_event(
                stdout,
                0,
                &EventKind::SessionCreated {
                    id: session_id.clone(),
                },
            )
            .await?;
            tracing::info!(session_id = %session_id, "NDJSON: session created");
            Ok(())
        }
        Command::SendUserMessage {
            session_id,
            text,
            attachments: _,
        } => handle_send_user_message(mgr, stdout, cmd_rx, session_id, text).await,
        Command::Cancel { session_id } => {
            mgr.cancel(&session_id).await?;
            // Runtime 会自动发 TurnEnd 事件（stop_reason=interrupted），
            // 由 handle_send_user_message 的事件 forwarder 转发到 stdout。
            // 若无在执行的 turn，Cancel 是 no-op（不发送任何事件）。
            Ok(())
        }
        Command::ListSessions => {
            let sessions = mgr.list_sessions();
            write_command(stdout, 0, &NdjsonCommandKind::SessionsListed { sessions }).await
        }
        Command::GetSession { session_id } => {
            let messages = mgr.get_messages(&session_id).await?;
            write_command(
                stdout,
                0,
                &NdjsonCommandKind::SessionRetrieved {
                    session_id,
                    messages,
                },
            )
            .await
        }
        Command::SetPermissionMode { session_id, mode } => {
            let session = mgr
                .get_or_load(&session_id)
                .await
                .map_err(|_| NdjsonError::SessionNotFound(session_id.clone()))?;
            let controller = session.runtime.plan_controller();
            controller.set_mode(mode).await;
            // Runtime 会自动发 PermissionModeChanged 事件（若有 active turn 的事件 forwarder，
            // 会转发；否则客户端无可见反馈——这是命令的语义：设置模式，不要求响应）。
            Ok(())
        }
        Command::ResolvePermission { id, decision } => {
            // ResolvePermission 在 protocol 中无 session_id 字段——
            // NDJSON 通常单会话，遍历所有会话查找 pending permission。
            // 找到则解析，找不到则发 CommandError。
            for meta in mgr.list_sessions() {
                if mgr
                    .resolve_permission(&meta.id, &id, decision.clone())
                    .await
                    .is_ok()
                {
                    return Ok(());
                }
            }
            write_command(
                stdout,
                0,
                &NdjsonCommandKind::CommandError {
                    message: format!("permission {id} not found in any session"),
                },
            )
            .await?;
            Ok(())
        }
        Command::Undo { session_id, steps } => {
            // FE-4（2026-08-26 R3 审查）：journal 已在 server 端注入
            // （runtime_builder §11b），与 HTTP `/undo` 行为对齐——原实现硬编码
            // "不支持"造成同一进程内 HTTP 可用、NDJSON 被拒的分裂。
            let Ok(session) = mgr.get_or_load(&session_id).await else {
                // 会话不存在：写错误响应后提前退出（let-else，clippy 手册推荐形态）
                write_command(
                    stdout,
                    0,
                    &NdjsonCommandKind::CommandError {
                        message: format!("session {session_id} not found"),
                    },
                )
                .await?;
                return Ok(());
            };
            let Some(journal) = session.runtime.journal() else {
                write_command(
                    stdout,
                    0,
                    &NdjsonCommandKind::CommandError {
                        message: "journal 未启用".to_string(),
                    },
                )
                .await?;
                return Ok(());
            };
            // 回滚与进行中的 turn 互斥（C-28/C-31，60s 上限对齐 HTTP 端点）。
            let undo =
                tokio::time::timeout(std::time::Duration::from_secs(60), session.turn_lock.lock())
                    .await;
            #[allow(clippy::single_match_else)] // 同上：错误分支写响应后提前退出
            let report = match undo {
                Ok(_guard) => journal.undo(steps.max(1)).await,
                Err(_) => {
                    write_command(
                        stdout,
                        0,
                        &NdjsonCommandKind::CommandError {
                            message: "会话忙：上一轮消息仍在处理中，请稍后再试".to_string(),
                        },
                    )
                    .await?;
                    return Ok(());
                }
            };
            match report {
                Ok(report) => {
                    // C-28：/undo 反向恢复落审计
                    let rec = minicoding_core::storage::AuditRecord {
                        ts: time::OffsetDateTime::now_utc(),
                        session: session.runtime.session().id.clone(),
                        kind: minicoding_core::storage::AuditKind::ToolCall,
                        tool: Some("undo".to_string()),
                        decision: None,
                        detail: format!(
                            "ndjson undo: steps={} undone_entries={} restored={} conflicts={}",
                            steps.max(1),
                            report.undone_entries,
                            report.restored_files.len(),
                            report.failed_files.len(),
                        ),
                    };
                    if let Err(e) = session.runtime.audit().record(rec).await {
                        tracing::warn!(error = %e, "ndjson undo audit failed");
                    }
                    write_command(
                        stdout,
                        0,
                        &NdjsonCommandKind::UndoReported {
                            undone_entries: report.undone_entries,
                            restored_files: report
                                .restored_files
                                .iter()
                                .map(std::string::ToString::to_string)
                                .collect(),
                            failed_files: report
                                .failed_files
                                .iter()
                                .map(|(p, reason)| {
                                    serde_json::json!({"path": p.to_string(), "reason": reason})
                                })
                                .collect(),
                        },
                    )
                    .await
                }
                Err(e) => {
                    write_command(
                        stdout,
                        0,
                        &NdjsonCommandKind::CommandError {
                            message: e.to_string(),
                        },
                    )
                    .await
                }
            }
        }
    }
}

/// 处理 `SendUserMessage`：后台 task 执行 `run_turn`，主循环转发事件到 stdout。
///
/// 设计要点：
/// - **订阅时机**：先订阅 `EventBus`，再 spawn `send_message_boxed`，避免错过早期事件；
/// - **select!**：在 `recv()` 与 `turn_task` 完成之间 select——turn 完成时 drain 剩余事件；
/// - **`TurnEnd`**：Runtime 在 `run_turn` 返回前发 `TurnEnd` 事件，forwarder 转发到 stdout；
/// - **错误**：turn 失败时（`TurnOutcome::Failed` 或 `RuntimeError`），发 `CommandError`。
async fn handle_send_user_message(
    mgr: Arc<SessionManager>,
    stdout: &SharedStdout,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<Command>,
    session_id: String,
    text: String,
) -> Result<(), NdjsonError> {
    let session = mgr
        .get_or_load(&session_id)
        .await
        .map_err(|_| NdjsonError::SessionNotFound(session_id.clone()))?;

    // 先订阅带 seq 的会话级事件流（FE-9，2026-08-28 R5 收尾）：此前 NDJSON
    // 自造每 turn seq=1..n，与 SSE/ACP/LSP 的会话级 cursor seq 空间不一致——
    // 跨协议切换/重连破坏游标语义。改走 `subscribe_sequenced`（seq 由会话
    // 常驻 sequencer 单一分配），与其余协议对齐。避免 spawn turn task 后错过
    // 早期事件（先订阅后 spawn）。
    let mut rx = session.subscribe_sequenced();

    // 后台 task 执行 send_message_boxed（持有 turn_lock，串行化）
    let mgr_clone = mgr.clone();
    let sid_clone = session_id.clone();
    let text_clone = text.clone();
    let mut turn_task = tokio::spawn(async move {
        SessionManager::send_message_boxed(mgr_clone, sid_clone, text_clone).await
    });

    // 转发事件，直到 turn_task 完成；turn 期间继续消费 stdin 命令
    // （`ResolvePermission` 唤醒权限等待、`Cancel` 中断 turn），
    // 否则客户端无法在 turn 进行中应答权限（死锁）。
    loop {
        tokio::select! {
            biased;
            turn_result = &mut turn_task => {
                // turn_task 完成：drain 剩余事件（TurnEnd 通常在 turn_task 返回前已发出）
                while let Ok((seq, kind)) = rx.try_recv() {
                    write_event(stdout, seq, &kind).await?;
                }
                // 根据 turn 结果发最终事件
                match turn_result {
                    // 正常完成 / 中断：`TurnEnd` 已在 drain 阶段转发
                    // （Runtime 在 `run_turn` 返回前发 `TurnEnd`，drain 阶段捕获）
                    Ok(Ok(TurnOutcome::Finished(_) | TurnOutcome::Interrupted(_))) => {}
                    Ok(Ok(TurnOutcome::Failed(e))) => {
                        write_command(stdout, 0, &NdjsonCommandKind::CommandError {
                            message: format!("turn failed: {e}"),
                        }).await?;
                    }
                    Ok(Err(e)) => {
                        write_command(stdout, 0, &NdjsonCommandKind::CommandError {
                            message: format!("turn error: {e}"),
                        }).await?;
                    }
                    Err(e) => {
                        write_command(stdout, 0, &NdjsonCommandKind::CommandError {
                            message: format!("turn task panicked: {e}"),
                        }).await?;
                    }
                }
                break;
            }
            next_cmd = cmd_rx.recv() => {
                if let Some(cmd) = next_cmd {
                    handle_turn_command(mgr.clone(), stdout, cmd).await?;
                } else {
                    // stdin EOF：客户端断连，取消当前 turn 并结束
                    tracing::info!(session_id = %session_id, "NDJSON stdin EOF during turn");
                    let _ = mgr.cancel(&session_id).await;
                    break;
                }
            }
            event_result = rx.recv() => {
                match event_result {
                    Ok((seq, kind)) => {
                        write_event(stdout, seq, &kind).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // 消费慢导致丢事件——E-14（2026-08-26 R3 审查落地）：
                        // 向客户端发 lag 通知，指示重拉 snapshot（GetSession）。
                        // 借 CommandError 通道（seq=0 advisory），NDJSON 无专用
                        // rehydrate 变体，协议扩展留待需要时。
                        tracing::warn!(session_id = %session_id, "NDJSON event consumer lagged");
                        let _ = write_command(
                            stdout,
                            0,
                            &NdjsonCommandKind::CommandError {
                                message: "event stream lagged: some events were dropped; \
                                          resend GetSession to re-sync snapshot"
                                    .to_string(),
                            },
                        )
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(())
}

/// 从 `SessionConfig`（客户端传入）与默认参数构造 `ServerRuntimeParams`。
///
/// 客户端可覆盖 `workdir`/`system`/`provider`/`model`/`permission_mode`；
/// `api_key`/`api_base`/`provider_name` 用 server 端默认（C-04：不通过 NDJSON 传凭证；
/// `provider_name` 为显示用，跟随 server 启动配置）。
fn build_params_from_config(
    default: &ServerRuntimeParams,
    config: minicoding_protocol::SessionConfig,
) -> ServerRuntimeParams {
    ServerRuntimeParams {
        provider_kind: config
            .provider
            .unwrap_or_else(|| default.provider_kind.clone()),
        provider_name: default.provider_name.clone(),
        api_base: default.api_base.clone(),
        api_key: default.api_key.clone(),
        model: config.model.unwrap_or_else(|| default.model.clone()),
        workdir: config
            .workdir
            .map_or_else(|| default.workdir.clone(), camino::Utf8PathBuf::from),
        system: config.system.or(default.system.clone()),
        permission_mode: config.permission_mode,
        sandbox_policy: default.sandbox_policy.clone(),
        timeout_sec: default.timeout_sec,
        max_retries: default.max_retries,
        small_model: default.small_model.clone(),
        turn_timeout_sec: default.turn_timeout_sec,
        compress: default.compress,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::policy::PermissionMode;
    use minicoding_core::sandbox::SandboxPolicy;
    use std::time::Duration;

    fn test_params() -> ServerRuntimeParams {
        ServerRuntimeParams {
            provider_kind: "openai".to_string(),
            provider_name: None,
            api_base: "http://localhost:8080/v1".to_string(),
            api_key: "sk-test".to_string(),
            model: "gpt-4o".to_string(),
            workdir: Utf8PathBuf::from("."),
            system: None,
            permission_mode: PermissionMode::Default,
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                workdir: Utf8PathBuf::from("."),
                writable: Vec::new(),
            },
            timeout_sec: 120,
            max_retries: 3,
            small_model: None,
            turn_timeout_sec: 600,
            compress: true,
        }
    }

    #[test]
    fn build_params_from_config_uses_defaults() {
        let default = test_params();
        let config = minicoding_protocol::SessionConfig::default();
        let params = build_params_from_config(&default, config);
        assert_eq!(params.provider_kind, "openai");
        assert_eq!(params.model, "gpt-4o");
        assert_eq!(params.api_key, "sk-test");
    }

    #[test]
    fn build_params_from_config_overrides_provider_and_model() {
        let default = test_params();
        let config = minicoding_protocol::SessionConfig {
            provider: Some("anthropic".to_string()),
            model: Some("claude-3-5-sonnet".to_string()),
            ..Default::default()
        };
        let params = build_params_from_config(&default, config);
        assert_eq!(params.provider_kind, "anthropic");
        assert_eq!(params.model, "claude-3-5-sonnet");
        // api_base/api_key 仍用 default（C-04：客户端不传凭证）
        assert_eq!(params.api_base, default.api_base);
        assert_eq!(params.api_key, default.api_key);
    }

    #[test]
    fn build_params_from_config_overrides_workdir_and_system() {
        let default = test_params();
        let config = minicoding_protocol::SessionConfig {
            workdir: Some("/tmp/work".to_string()),
            system: Some("custom system prompt".to_string()),
            ..Default::default()
        };
        let params = build_params_from_config(&default, config);
        assert_eq!(params.workdir, Utf8PathBuf::from("/tmp/work"));
        assert_eq!(params.system.as_deref(), Some("custom system prompt"));
    }

    /// 测试 helper：新建空 channel 并调用 `dispatch_command`。
    async fn dispatch_with(
        mgr: &Arc<SessionManager>,
        stdout: &SharedStdout,
        cmd: Command,
    ) -> Result<(), NdjsonError> {
        let (_tx, mut rx) = tokio::sync::mpsc::channel::<Command>(16);
        dispatch_command(mgr.clone(), stdout, &mut rx, cmd).await
    }

    #[tokio::test]
    async fn dispatch_list_sessions_emits_sessions_listed() {
        let mgr = Arc::new(SessionManager::new(test_params(), Duration::from_secs(5)));
        // 用内存 stdout 模拟（实际生产用 tokio::io::stdout）
        let stdout: SharedStdout = Arc::new(Mutex::new(tokio::io::BufWriter::new(Box::new(
            tokio::io::sink(),
        ))));

        let result = dispatch_with(&mgr, &stdout, Command::ListSessions).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_get_session_for_nonexistent_returns_error() {
        let mgr = Arc::new(SessionManager::new(test_params(), Duration::from_secs(5)));
        let stdout: SharedStdout = Arc::new(Mutex::new(tokio::io::BufWriter::new(Box::new(
            tokio::io::sink(),
        ))));

        let result = dispatch_with(
            &mgr,
            &stdout,
            Command::GetSession {
                session_id: "nonexistent".to_string(),
            },
        )
        .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            NdjsonError::Session(_) => {}
            other => panic!("expected Session error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_cancel_nonexistent_returns_error() {
        let mgr = Arc::new(SessionManager::new(test_params(), Duration::from_secs(5)));
        let stdout: SharedStdout = Arc::new(Mutex::new(tokio::io::BufWriter::new(Box::new(
            tokio::io::sink(),
        ))));

        let result = dispatch_with(
            &mgr,
            &stdout,
            Command::Cancel {
                session_id: "nonexistent".to_string(),
            },
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_undo_emits_command_error() {
        let mgr = Arc::new(SessionManager::new(test_params(), Duration::from_secs(5)));
        let stdout: SharedStdout = Arc::new(Mutex::new(tokio::io::BufWriter::new(Box::new(
            tokio::io::sink(),
        ))));

        // Undo 应返回 Ok（已发 CommandError 事件），不返回 Err
        let result = dispatch_with(
            &mgr,
            &stdout,
            Command::Undo {
                session_id: "test".to_string(),
                steps: 1,
            },
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_resolve_permission_nonexistent_returns_ok_with_command_error_event() {
        // ResolvePermission 在 pending 不存在时返回 Ok（已发 CommandError 事件）
        let mgr = Arc::new(SessionManager::new(test_params(), Duration::from_secs(5)));
        let stdout: SharedStdout = Arc::new(Mutex::new(tokio::io::BufWriter::new(Box::new(
            tokio::io::sink(),
        ))));

        let result = dispatch_with(
            &mgr,
            &stdout,
            Command::ResolvePermission {
                id: "nonexistent".to_string(),
                decision: minicoding_core::policy::Decision::Allow,
            },
        )
        .await;
        assert!(result.is_ok());
    }
}
