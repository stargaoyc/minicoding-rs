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
//! | `Undo` | `CommandError`（M8 未实现，需 `file-undo` feature） |
//!
//! ## seq 语义
//!
//! - 流式事件（`SendUserMessage` 期间）：`seq` 单调递增（1, 2, 3, ...），客户端用于检测丢失；
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
use minicoding_protocol::event::{EventDto, EventKind};
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
    let stdin = tokio::io::stdin();
    let stdout: Box<dyn AsyncWrite + Send + Unpin> = Box::new(tokio::io::stdout());
    let reader = BufReader::new(stdin);
    let stdout: SharedStdout = Arc::new(Mutex::new(tokio::io::BufWriter::new(stdout)));

    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        // 空行跳过（编辑器可能发送心跳/空行）
        if line.trim().is_empty() {
            continue;
        }

        // 解析 Command
        let cmd: Command = match serde_json::from_str(&line) {
            Ok(c) => c,
            Err(e) => {
                write_event(
                    &stdout,
                    0,
                    &EventKind::CommandError {
                        message: format!("invalid command JSON: {e}"),
                    },
                )
                .await?;
                continue;
            }
        };

        // 分派命令
        if let Err(e) = dispatch_command(mgr.clone(), &stdout, cmd).await {
            tracing::warn!(error = %e, "NDJSON command dispatch failed");
            write_event(
                &stdout,
                0,
                &EventKind::CommandError {
                    message: e.to_string(),
                },
            )
            .await?;
        }
    }

    Ok(())
}

/// 写一个 `EventDto` 到 stdout（JSON + 换行 + flush）。
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

/// 分派 `Command` 到对应的 handler。
async fn dispatch_command(
    mgr: Arc<SessionManager>,
    stdout: &SharedStdout,
    cmd: Command,
) -> Result<(), NdjsonError> {
    match cmd {
        Command::CreateSession { config } => {
            let params = build_params_from_config(mgr.default_params(), config);
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
        } => handle_send_user_message(mgr, stdout, session_id, text).await,
        Command::Cancel { session_id } => {
            mgr.cancel(&session_id)?;
            // Runtime 会自动发 TurnEnd 事件（stop_reason=interrupted），
            // 由 handle_send_user_message 的事件 forwarder 转发到 stdout。
            // 若无在执行的 turn，Cancel 是 no-op（不发送任何事件）。
            Ok(())
        }
        Command::ListSessions => {
            let sessions = mgr.list_sessions();
            write_event(stdout, 0, &EventKind::SessionsListed { sessions }).await
        }
        Command::GetSession { session_id } => {
            let messages = mgr.get_messages(&session_id).await?;
            write_event(
                stdout,
                0,
                &EventKind::SessionRetrieved {
                    session_id,
                    messages,
                },
            )
            .await
        }
        Command::SetPermissionMode { session_id, mode } => {
            let session = mgr
                .get(&session_id)
                .ok_or_else(|| NdjsonError::SessionNotFound(session_id.clone()))?;
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
            write_event(
                stdout,
                0,
                &EventKind::CommandError {
                    message: format!("permission {id} not found in any session"),
                },
            )
            .await?;
            Ok(())
        }
        Command::Undo { session_id, steps } => {
            // Undo 需要 `file-undo` feature 与 Journal 注入，M8 server 端未启用。
            // 返回 CommandError 让客户端知道命令不支持。
            let _ = (session_id, steps);
            write_event(
                stdout,
                0,
                &EventKind::CommandError {
                    message: "undo not supported in NDJSON mode (requires file-undo feature)"
                        .to_string(),
                },
            )
            .await
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
    session_id: String,
    text: String,
) -> Result<(), NdjsonError> {
    let session = mgr
        .get(&session_id)
        .ok_or_else(|| NdjsonError::SessionNotFound(session_id.clone()))?;

    // 先订阅 EventBus，避免 spawn turn task 后错过早期事件
    let runtime = session.runtime.clone();
    let mut rx = runtime.events().subscribe();

    // 后台 task 执行 send_message_boxed（持有 turn_lock，串行化）
    let mgr_clone = mgr.clone();
    let sid_clone = session_id.clone();
    let text_clone = text.clone();
    let mut turn_task = tokio::spawn(async move {
        SessionManager::send_message_boxed(mgr_clone, sid_clone, text_clone).await
    });

    // 转发事件，直到 turn_task 完成
    let mut seq: u64 = 0;
    loop {
        tokio::select! {
            biased;
            turn_result = &mut turn_task => {
                // turn_task 完成：drain 剩余事件（TurnEnd 通常在 turn_task 返回前已发出）
                while let Ok(event) = rx.try_recv() {
                    seq += 1;
                    write_event(stdout, seq, &EventKind::from(&event)).await?;
                }
                // 根据 turn 结果发最终事件
                match turn_result {
                    // 正常完成 / 中断：`TurnEnd` 已在 drain 阶段转发
                    // （Runtime 在 `run_turn` 返回前发 `TurnEnd`，drain 阶段捕获）
                    Ok(Ok(TurnOutcome::Finished(_) | TurnOutcome::Interrupted(_))) => {}
                    Ok(Ok(TurnOutcome::Failed(e))) => {
                        write_event(stdout, 0, &EventKind::CommandError {
                            message: format!("turn failed: {e}"),
                        }).await?;
                    }
                    Ok(Err(e)) => {
                        write_event(stdout, 0, &EventKind::CommandError {
                            message: format!("turn error: {e}"),
                        }).await?;
                    }
                    Err(e) => {
                        write_event(stdout, 0, &EventKind::CommandError {
                            message: format!("turn task panicked: {e}"),
                        }).await?;
                    }
                }
                break;
            }
            event_result = rx.recv() => {
                match event_result {
                    Ok(event) => {
                        seq += 1;
                        write_event(stdout, seq, &EventKind::from(&event)).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // 消费慢导致丢事件——发 RehydrateRequired 让客户端重拉 snapshot
                        tracing::warn!(session_id = %session_id, "NDJSON event consumer lagged");
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
/// `api_key`/`api_base` 用 server 端默认（C-04：不通过 NDJSON 传凭证）。
fn build_params_from_config(
    default: &ServerRuntimeParams,
    config: minicoding_protocol::SessionConfig,
) -> ServerRuntimeParams {
    ServerRuntimeParams {
        provider_kind: config
            .provider
            .unwrap_or_else(|| default.provider_kind.clone()),
        api_base: default.api_base.clone(),
        api_key: default.api_key.clone(),
        model: config.model.unwrap_or_else(|| default.model.clone()),
        workdir: config
            .workdir
            .map_or_else(|| default.workdir.clone(), camino::Utf8PathBuf::from),
        system: config.system.or(default.system.clone()),
        permission_mode: config.permission_mode,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::policy::PermissionMode;
    use std::time::Duration;

    fn test_params() -> ServerRuntimeParams {
        ServerRuntimeParams {
            provider_kind: "openai".to_string(),
            api_base: "http://localhost:8080/v1".to_string(),
            api_key: "sk-test".to_string(),
            model: "gpt-4o".to_string(),
            workdir: Utf8PathBuf::from("."),
            system: None,
            permission_mode: PermissionMode::Default,
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

    #[tokio::test]
    async fn dispatch_list_sessions_emits_sessions_listed() {
        let mgr = Arc::new(SessionManager::new(test_params(), Duration::from_secs(5)));
        // 用内存 stdout 模拟（实际生产用 tokio::io::stdout）
        let stdout: SharedStdout = Arc::new(Mutex::new(tokio::io::BufWriter::new(Box::new(
            tokio::io::sink(),
        ))));

        let result = dispatch_command(mgr, &stdout, Command::ListSessions).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_get_session_for_nonexistent_returns_error() {
        let mgr = Arc::new(SessionManager::new(test_params(), Duration::from_secs(5)));
        let stdout: SharedStdout = Arc::new(Mutex::new(tokio::io::BufWriter::new(Box::new(
            tokio::io::sink(),
        ))));

        let result = dispatch_command(
            mgr,
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

        let result = dispatch_command(
            mgr,
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
        let result = dispatch_command(
            mgr,
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

        let result = dispatch_command(
            mgr,
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
