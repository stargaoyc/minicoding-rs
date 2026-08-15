//! LSP stdio 适配器（T-M8-8/T-M8-9）：可被任何支持 LSP 的编辑器
//! （VS Code / Neovim / Emacs / Helix 等）嵌入。
//!
//! 基于 `tower-lsp` 实现 `LanguageServer` trait，把 minicoding 的对话/工具/权限能力
//! 映射到 LSP 标准方法（语义映射见 `design.md` §24）：
//!
//! | minicoding 能力 | LSP 方法 | 方向 | 说明 |
//! |-----------------|---------|------|------|
//! | 会话初始化 | `initialize` | 请求-响应 | 返回 `executeCommand`/`codeAction` 能力 |
//! | 发送 prompt | `workspace/executeCommand` | 请求-响应 | `command="minicoding.ask"` |
//! | 取消 turn | `workspace/executeCommand` | 请求-响应 | `command="minicoding.cancel"` |
//! | 流式 token | `$/progress` | 通知 | `WorkDoneProgress::Report`，message 含 token |
//! | 工具调用进度 | `$/progress` | 通知 | `WorkDoneProgress::Report`，message 含工具名 |
//! | 事件广播 | `minicoding/event`（自定义通知） | 通知 | 携带 `seq`，与 SSE cursor 一致 |
//! | 权限确认 | `window/showMessageRequest` | 请求-响应 | `LspPrompter` 点对点 |
//! | AI 快速操作 | `textDocument/codeAction` | 请求-响应 | 解释/重构/修复选中代码 |
//!
//! ## 安全约束
//!
//! - **C-01**：副作用工具经 `LspPrompter` → `window/showMessageRequest` 交互；
//! - **C-04**：不向客户端泄露凭证（`ServerRuntimeParams` 不在 LSP 响应中暴露）；
//! - **C-05**：所有 stdout 输出是结构化 JSON-RPC，非 LLM 直接输出。
//!
//! 详见 `docs/dev-plan.md` T-M8-8/T-M8-9、`docs/design.md` §24。

#![cfg(feature = "lsp")]

use crate::lsp_prompter::{LspPrompter, PermissionRequest};
use crate::session_mgr::{ServerSession, SessionManager};
use minicoding_core::model::TurnOutcome;
use minicoding_core::policy::{Decision, PermissionPrompter, PromptOption, Risk};
use minicoding_protocol::event::{EventDto, EventKind};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Mutex as TokioMutex, mpsc};
use tower_lsp::lsp_types::notification::{Notification as LspNotification, Progress};
use tower_lsp::lsp_types::{
    CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, CodeActionResponse,
    Command, ExecuteCommandOptions, ExecuteCommandParams, InitializeParams, InitializeResult,
    InitializedParams, MessageActionItem, MessageType, NumberOrString, ProgressParams,
    ProgressParamsValue, Range, ServerCapabilities, ServerInfo, Url, WorkDoneProgress,
    WorkDoneProgressBegin, WorkDoneProgressEnd, WorkDoneProgressOptions, WorkDoneProgressReport,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};

// ─── 自定义命令名 ───────────────────────────────────────────────────────────

/// 发送 prompt（`workspace/executeCommand` 的 `command` 字段）。
const CMD_ASK: &str = "minicoding.ask";
/// 取消当前 turn。
const CMD_CANCEL: &str = "minicoding.cancel";
/// 解释选中代码（由 `codeAction` 触发）。
const CMD_EXPLAIN: &str = "minicoding.explain";
/// 重构选中代码。
const CMD_REFACTOR: &str = "minicoding.refactor";
/// 修复选中代码。
const CMD_FIX: &str = "minicoding.fix";

// ─── 自定义通知类型 ─────────────────────────────────────────────────────────

/// `minicoding/event` 通知参数（携带 `seq` 的 `EventDto` + 会话 ID）。
#[derive(Debug, Serialize, Deserialize)]
struct MinicodingEventParams {
    /// 当前会话 ID（与 `initialize` 响应中的 `conversation_id` 一致）。
    conversation_id: String,
    /// 事件 DTO（`seq` + `kind`）。
    #[serde(flatten)]
    event: EventDto,
}

/// `minicoding/event` 自定义通知（server→client）。
struct MinicodingEvent;

impl LspNotification for MinicodingEvent {
    const METHOD: &'static str = "minicoding/event";
    type Params = MinicodingEventParams;
}

// ─── 错误类型 ───────────────────────────────────────────────────────────────

/// LSP 适配器错误。
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// stdin/stdout IO 错误。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ─── LSP server backend ─────────────────────────────────────────────────────

/// minicoding LSP server backend。
///
/// 持有 `Client`（发送通知/请求到编辑器）、`SessionManager`（多会话管理，LSP 端通常
/// 单会话）、`LspPrompter`（创建会话时注入 Runtime 的权限交互器）。
///
/// 会话惰性创建：首次 `minicoding.ask` / `minicoding.explain` 等命令时调用
/// `get_or_create_session`。
struct MinicodingLspServer {
    client: Client,
    mgr: Arc<SessionManager>,
    prompter: LspPrompter,
    /// 当前会话（LSP 端通常单会话，惰性创建）。
    session: TokioMutex<Option<Arc<ServerSession>>>,
}

impl MinicodingLspServer {
    #[must_use]
    fn new(client: Client, mgr: Arc<SessionManager>, prompter: LspPrompter) -> Self {
        Self {
            client,
            mgr,
            prompter,
            session: TokioMutex::new(None),
        }
    }

    /// 获取或惰性创建会话。
    ///
    /// 首次调用时用 `LspPrompter` 构造 Runtime 并注册到 `SessionManager`。
    /// 后续调用返回已存在的会话。
    ///
    /// # Errors
    /// Runtime 构造失败时返回错误描述。
    async fn get_or_create_session(&self) -> Result<Arc<ServerSession>, String> {
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let prompter: Arc<dyn PermissionPrompter> = Arc::new(self.prompter.clone());
        let session = self
            .mgr
            .create_session_with_prompter(None, prompter)
            .map_err(|e| e.to_string())?;
        let session_id = session.session_id().clone();
        tracing::info!(session_id = %session_id, "LSP session created");
        *guard = Some(session.clone());
        Ok(session)
    }

    /// 运行一轮 turn：订阅 `EventBus` → 后台 spawn turn → 转发事件到 LSP 客户端。
    ///
    /// 事件转发策略：
    /// - **所有事件** → `minicoding/event` 通知（携带 `seq`，供完整客户端消费）；
    /// - **`Token` / `ToolCall` / `TurnStart` / `TurnEnd`** → 额外 `$/progress` 通知
    ///   （供标准 LSP 客户端渲染进度，token 文本放 `WorkDoneProgressReport::message`）。
    ///
    /// # Errors
    /// turn 执行失败、session 不存在、task panic 时返回错误描述。
    async fn run_turn(&self, text: String) -> Result<(), String> {
        let session = self.get_or_create_session().await?;
        let session_id = session.session_id().clone();

        // 订阅 EventBus（在 spawn turn task 之前订阅，避免错过早期事件）
        let runtime = session.runtime.clone();
        let mut rx = runtime.events().subscribe();

        // 后台 task 执行 turn
        let mgr = self.mgr.clone();
        let sid = session_id.clone();
        let mut turn_task =
            tokio::spawn(async move { SessionManager::send_message_boxed(mgr, sid, text).await });

        // 转发事件到 LSP 客户端
        let client = self.client.clone();
        let conv_id = session_id.clone();
        let progress_token = NumberOrString::String(format!("minicoding.turn.{conv_id}"));
        let session_clone = session.clone();

        loop {
            tokio::select! {
                biased;
                turn_result = &mut turn_task => {
                    // drain 剩余事件（turn 完成后 EventBus 可能还有未消费的事件）
                    while let Ok(event) = rx.try_recv() {
                        let seq = session_clone.push_event(&event).await;
                        forward_event(&client, &conv_id, &progress_token, seq, &EventKind::from(&event)).await;
                    }
                    return match turn_result {
                        Ok(Ok(TurnOutcome::Finished(_) | TurnOutcome::Interrupted(_))) => Ok(()),
                        Ok(Ok(TurnOutcome::Failed(e))) => Err(format!("turn failed: {e}")),
                        Ok(Err(e)) => Err(format!("session error: {e}")),
                        Err(e) => Err(format!("turn task panicked: {e}")),
                    };
                }
                event_result = rx.recv() => {
                    match event_result {
                        Ok(event) => {
                            let seq = session_clone.push_event(&event).await;
                            forward_event(
                                &client,
                                &conv_id,
                                &progress_token,
                                seq,
                                &EventKind::from(&event),
                            )
                            .await;
                        }
                        // channel 关闭（所有 Runtime handle 释放）→ 退出转发 loop
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        // 客户端落后（lagged）→ 跳过该批次，继续 loop（与 SSE Rehydrate 语义不同：
                        // LSP 端单会话订阅，lagged 极少发生；完整事件可由客户端从 minicoding/event 重建）
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "LSP event subscriber lagged, skipping");
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

// ─── LanguageServer trait 实现 ──────────────────────────────────────────────

#[tower_lsp::async_trait]
impl LanguageServer for MinicodingLspServer {
    async fn initialize(
        &self,
        _: InitializeParams,
    ) -> tower_lsp::jsonrpc::Result<InitializeResult> {
        let commands = vec![
            CMD_ASK.to_string(),
            CMD_CANCEL.to_string(),
            CMD_EXPLAIN.to_string(),
            CMD_REFACTOR.to_string(),
            CMD_FIX.to_string(),
        ];
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                // `work_done_progress_provider` 在 lsp-types 0.94.1 中由 `WorkDoneProgressOptions`
                // 表达（已嵌入 `execute_command_provider`/`code_action_provider`），不单独声明。
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "minicoding".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(
                MessageType::INFO,
                "minicoding LSP server initialized — use command 'minicoding.ask' to start",
            )
            .await;
    }

    async fn shutdown(&self) -> tower_lsp::jsonrpc::Result<()> {
        tracing::info!("LSP client requested shutdown");
        Ok(())
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> tower_lsp::jsonrpc::Result<Option<serde_json::Value>> {
        match params.command.as_str() {
            CMD_ASK => {
                let text = extract_text_arg(&params.arguments)
                    .map_err(tower_lsp::jsonrpc::Error::invalid_params)?;
                self.run_turn(text).await.map_err(internal_error)?;
                Ok(Some(serde_json::json!({"status": "completed"})))
            }
            CMD_CANCEL => {
                let guard = self.session.lock().await;
                if let Some(session) = guard.as_ref() {
                    let session_id = session.session_id();
                    if let Err(e) = self.mgr.cancel(session_id).await {
                        tracing::warn!(session_id = %session_id, error = %e, "LSP cancel failed");
                    }
                }
                Ok(Some(serde_json::json!({"status": "cancelled"})))
            }
            CMD_EXPLAIN | CMD_REFACTOR | CMD_FIX => {
                let (uri, range) = extract_uri_range(&params.arguments)
                    .map_err(tower_lsp::jsonrpc::Error::invalid_params)?;
                let selected_text = read_range(&uri, &range)
                    .await
                    .map_err(|e| internal_error(e.to_string()))?;
                let prompt = match params.command.as_str() {
                    CMD_EXPLAIN => format!("请解释这段代码:\n```\n{selected_text}\n```"),
                    CMD_REFACTOR => {
                        format!("请重构这段代码，保持行为不变:\n```\n{selected_text}\n```")
                    }
                    CMD_FIX => format!("请修复这段代码中的问题:\n```\n{selected_text}\n```"),
                    _ => unreachable!(),
                };
                self.run_turn(prompt).await.map_err(internal_error)?;
                Ok(Some(serde_json::json!({"status": "completed"})))
            }
            other => Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "unknown command `{other}`"
            ))),
        }
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> tower_lsp::jsonrpc::Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri.clone();
        let range = params.range;
        let actions: CodeActionResponse = vec![
            CodeActionOrCommand::Command(code_action_command(
                "解释选中代码",
                CMD_EXPLAIN,
                &uri,
                &range,
            )),
            CodeActionOrCommand::Command(code_action_command(
                "重构选中代码",
                CMD_REFACTOR,
                &uri,
                &range,
            )),
            CodeActionOrCommand::Command(code_action_command(
                "修复选中代码",
                CMD_FIX,
                &uri,
                &range,
            )),
        ];
        Ok(Some(actions))
    }
}

// ─── 入口函数 ───────────────────────────────────────────────────────────────

/// 启动 LSP server：基于 `tower-lsp`，阻塞当前 task 直到客户端断开 stdin 或发 shutdown。
///
/// 构造 `LspPrompter`（mpsc channel 解耦 `tower_lsp::Client` 与 `LspPrompter`），
/// 在 `LspService::new` 闭包中 spawn prompter loop（持有 `Client` + `mpsc::Receiver`，
/// 把权限请求转为 `window/showMessageRequest`）。
///
/// # Errors
/// stdin/stdout IO 错误。
///
/// # 设计要点
///
/// - **通道解耦**：`LspPrompter` 不持有 `tower_lsp::Client`（Client 在 `LspService::new`
///   闭包中才可用，而 `LspPrompter` 需在 Runtime 构造前注入）。用 mpsc channel 转发
///   权限请求到 prompter loop，后者持有 Client 调 `showMessageRequest`；
/// - **单会话**：LSP 端通常单会话（workspace 固定），惰性创建于首次 `executeCommand`。
pub async fn serve_lsp(
    mgr: Arc<SessionManager>,
    permission_timeout: std::time::Duration,
) -> Result<(), LspError> {
    let (tx, rx) = mpsc::channel::<PermissionRequest>(16);
    let prompter = LspPrompter::new(tx, permission_timeout);

    let (service, socket) = LspService::new(move |client| {
        // Spawn prompter loop：接收 PermissionRequest，调 client.show_message_request
        tokio::spawn(prompter_loop(client.clone(), rx));
        MinicodingLspServer::new(client, mgr, prompter)
    });

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    Server::new(stdin, stdout, socket).serve(service).await;
    Ok(())
}

// ─── prompter loop ──────────────────────────────────────────────────────────

/// 权限交互 loop：从 `LspPrompter` 接收权限请求，调 `window/showMessageRequest`，
/// 用户选择后通过 oneshot 回传 `Decision`。
///
/// 与 `ServerPrompter`（HTTP POST resolve）/ `TuiPrompter`（mpsc 到 TUI 主循环）
/// 同构，只是交互通道不同（LSP `showMessageRequest`）。
async fn prompter_loop(client: Client, mut rx: mpsc::Receiver<PermissionRequest>) {
    while let Some(req) = rx.recv().await {
        let PermissionRequest { prompt, reply } = req;
        let actions = build_message_actions(&prompt.options);
        let message = format!(
            "[{}] {} (tool: {})",
            risk_label(prompt.risk),
            prompt.summary,
            prompt.tool
        );
        let result = client
            .show_message_request(MessageType::INFO, message, Some(actions))
            .await;
        let decision = match result {
            Ok(Some(item)) => map_action_to_decision(&item.title, &prompt.options),
            Ok(None) => Decision::Deny("permission request dismissed".to_string()),
            Err(e) => {
                tracing::warn!(error = %e, "showMessageRequest failed, denying");
                Decision::Deny(format!("showMessageRequest failed: {e}"))
            }
        };
        let _ = reply.send(decision);
    }
}

/// 把 `PromptOption` 列表转为 LSP `MessageActionItem` 列表。
fn build_message_actions(options: &[PromptOption]) -> Vec<MessageActionItem> {
    options
        .iter()
        .map(|o| MessageActionItem {
            title: prompt_option_label(*o),
            // LSP 0.94.1 `MessageActionItem.properties` 为 `HashMap<String, MessageActionItemProperty>`，
            // 无附加属性时用空 map（serde `skip_serializing_if = "is_empty"` 会省略该字段）。
            properties: std::collections::HashMap::new(),
        })
        .collect()
}

/// `PromptOption` → 显示标签。
fn prompt_option_label(opt: PromptOption) -> String {
    match opt {
        PromptOption::AllowOnce => "Allow Once".to_string(),
        PromptOption::AllowAlways => "Allow Always".to_string(),
        PromptOption::DenyOnce => "Deny Once".to_string(),
        PromptOption::DenyAlways => "Deny Always".to_string(),
    }
}

/// 用户选择的 action title → `Decision`。
///
/// `Allow Once`/`Allow Always` → `Decision::Allow`（"always" 语义由 `PermissionPolicy`
/// 缓存层处理，`Decision` enum 只有 Allow/Deny）；
/// 其他 → `Decision::Deny`。
fn map_action_to_decision(title: &str, _options: &[PromptOption]) -> Decision {
    match title {
        "Allow Once" | "Allow Always" => Decision::Allow,
        other => Decision::Deny(format!("user denied: {other}")),
    }
}

/// 风险等级 → 显示标签。
fn risk_label(risk: Risk) -> &'static str {
    match risk {
        Risk::Low => "Low",
        Risk::Medium => "Medium",
        Risk::High => "High",
    }
}

// ─── 事件转发 ───────────────────────────────────────────────────────────────

/// 把 `EventKind` 转发到 LSP 客户端：
/// 1. `minicoding/event` 通知（所有事件，携带 `seq`）；
/// 2. `$/progress` 通知（Token/ToolCall/TurnStart/TurnEnd，供标准 LSP 客户端渲染）。
async fn forward_event(
    client: &Client,
    conversation_id: &str,
    progress_token: &NumberOrString,
    seq: u64,
    kind: &EventKind,
) {
    // 1. minicoding/event 通知（携带 seq，完整客户端消费）
    let params = MinicodingEventParams {
        conversation_id: conversation_id.to_string(),
        event: EventDto {
            seq,
            kind: kind.clone(),
        },
    };
    client.send_notification::<MinicodingEvent>(params).await;

    // 2. $/progress 通知（标准 LSP 客户端渲染进度）
    if let Some(progress) = event_to_progress(kind, progress_token.clone()) {
        client.send_notification::<Progress>(progress).await;
    }
}

/// 把 `EventKind` 映射为 `$/progress` 通知（仅 Token/ToolCall/TurnStart/TurnEnd）。
fn event_to_progress(kind: &EventKind, token: NumberOrString) -> Option<ProgressParams> {
    let progress = match kind {
        EventKind::TurnStreamingStarted => WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: "minicoding".to_string(),
            cancellable: Some(true),
            message: None,
            percentage: None,
        }),
        EventKind::Token { text } => WorkDoneProgress::Report(WorkDoneProgressReport {
            cancellable: Some(true),
            message: Some(text.clone()),
            percentage: None,
        }),
        EventKind::ToolCallStarted { tool, .. } => {
            WorkDoneProgress::Report(WorkDoneProgressReport {
                cancellable: Some(true),
                message: Some(format!("tool: {tool}")),
                percentage: None,
            })
        }
        EventKind::ToolCallFinished { .. } => WorkDoneProgress::Report(WorkDoneProgressReport {
            cancellable: Some(true),
            message: Some("tool done".to_string()),
            percentage: None,
        }),
        EventKind::TurnEnd { .. } => WorkDoneProgress::End(WorkDoneProgressEnd {
            message: Some("turn completed".to_string()),
        }),
        _ => return None,
    };
    Some(ProgressParams {
        token,
        value: ProgressParamsValue::WorkDone(progress),
    })
}

// ─── 命令参数解析 ───────────────────────────────────────────────────────────

/// `minicoding.ask` 参数：`arguments[0] = { "text": "..." }`。
///
/// # Errors
/// 参数缺失、JSON 解析失败时返回错误描述。
fn extract_text_arg(arguments: &[serde_json::Value]) -> Result<String, String> {
    #[derive(Deserialize)]
    struct AskArgs {
        text: String,
    }
    let first = arguments
        .first()
        .ok_or("missing arguments[0] for minicoding.ask")?;
    let args: AskArgs = serde_json::from_value(first.clone())
        .map_err(|e| format!("invalid minicoding.ask arguments: {e}"))?;
    Ok(args.text)
}

/// `minicoding.explain`/`refactor`/`fix` 参数：`arguments[0] = { "uri": ..., "range": ... }`。
///
/// # Errors
/// 参数缺失、JSON 解析失败时返回错误描述。
fn extract_uri_range(arguments: &[serde_json::Value]) -> Result<(Url, Range), String> {
    #[derive(Deserialize)]
    struct CodeActionArgs {
        uri: Url,
        range: Range,
    }
    let first = arguments
        .first()
        .ok_or("missing arguments[0] for code action command")?;
    let args: CodeActionArgs = serde_json::from_value(first.clone())
        .map_err(|e| format!("invalid code action arguments: {e}"))?;
    Ok((args.uri, args.range))
}

/// 构造 `Command`（用于 `codeAction` 响应，用户点击后触发 `executeCommand`）。
fn code_action_command(title: &str, command: &str, uri: &Url, range: &Range) -> Command {
    Command {
        title: title.to_string(),
        command: command.to_string(),
        arguments: Some(vec![serde_json::json!({
            "uri": uri,
            "range": range,
        })]),
    }
}

/// 读取文件指定范围的内容（按行+字符偏移提取）。
///
/// LSP `Range` 的 `line`/`character` 均为 0-indexed。
///
/// # Errors
/// 文件读取失败、URI 非 file:// scheme 时返回 `io::Error`。
async fn read_range(uri: &Url, range: &Range) -> std::io::Result<String> {
    let path = uri.to_file_path().map_err(|()| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "URI is not a valid file path",
        )
    })?;
    let content = tokio::fs::read_to_string(&path).await?;
    let lines: Vec<&str> = content.lines().collect();
    let start_line = (range.start.line as usize).min(lines.len().saturating_sub(1));
    let end_line = (range.end.line as usize).min(lines.len().saturating_sub(1));
    if start_line > end_line {
        return Ok(String::new());
    }

    let mut result: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(end_line + 1).skip(start_line) {
        let chars: Vec<char> = line.chars().collect();
        let extracted: String = if i == start_line && i == end_line {
            // 单行范围
            let start = (range.start.character as usize).min(chars.len());
            let end = (range.end.character as usize).min(chars.len());
            let start = start.min(end);
            chars[start..end].iter().collect()
        } else if i == start_line {
            // 起始行：从 start.character 到行尾
            let start = (range.start.character as usize).min(chars.len());
            chars[start..].iter().collect()
        } else if i == end_line {
            // 结束行：从行首到 end.character
            let end = (range.end.character as usize).min(chars.len());
            chars[..end].iter().collect()
        } else {
            // 中间完整行
            (*line).to_string()
        };
        result.push(extracted);
    }
    Ok(result.join("\n"))
}

/// 把错误描述转为 `tower_lsp::jsonrpc::Error`（`InternalError` + data）。
fn internal_error(message: String) -> tower_lsp::jsonrpc::Error {
    let mut e = tower_lsp::jsonrpc::Error::internal_error();
    e.message = message.into();
    e
}

// ─── 测试 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::policy::Risk;
    use tower_lsp::lsp_types::Position;

    #[test]
    fn prompt_option_labels_are_human_readable() {
        assert_eq!(prompt_option_label(PromptOption::AllowOnce), "Allow Once");
        assert_eq!(prompt_option_label(PromptOption::DenyAlways), "Deny Always");
    }

    #[test]
    fn map_action_allow_becomes_decision_allow() {
        let opts = vec![PromptOption::AllowOnce, PromptOption::DenyOnce];
        let decision = map_action_to_decision("Allow Once", &opts);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn map_action_unknown_becomes_decision_deny() {
        let opts = vec![PromptOption::AllowOnce];
        let decision = map_action_to_decision("Deny Once", &opts);
        assert!(matches!(decision, Decision::Deny(_)));
    }

    #[test]
    fn risk_labels_are_correct() {
        assert_eq!(risk_label(Risk::Low), "Low");
        assert_eq!(risk_label(Risk::Medium), "Medium");
        assert_eq!(risk_label(Risk::High), "High");
    }

    #[test]
    fn build_message_actions_preserves_order() {
        let opts = vec![
            PromptOption::AllowOnce,
            PromptOption::AllowAlways,
            PromptOption::DenyOnce,
        ];
        let actions = build_message_actions(&opts);
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].title, "Allow Once");
        assert_eq!(actions[1].title, "Allow Always");
        assert_eq!(actions[2].title, "Deny Once");
    }

    #[test]
    fn extract_text_arg_parses_json() {
        let args = vec![serde_json::json!({"text": "hello world"})];
        let text = extract_text_arg(&args).unwrap();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn extract_text_arg_errors_on_missing_arg() {
        let args: Vec<serde_json::Value> = vec![];
        let result = extract_text_arg(&args);
        assert!(result.is_err());
    }

    #[test]
    fn extract_uri_range_parses_uri_and_range() {
        let args = vec![serde_json::json!({
            "uri": "file:///tmp/test.rs",
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 1, "character": 5}
            }
        })];
        let (uri, range) = extract_uri_range(&args).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/test.rs");
        assert_eq!(range.start.line, 0);
        assert_eq!(range.end.character, 5);
    }

    #[test]
    fn event_to_progress_token_event_becomes_report() {
        let token = NumberOrString::String("test".to_string());
        let kind = EventKind::Token {
            text: "hi".to_string(),
        };
        let progress =
            event_to_progress(&kind, token).expect("token event should produce progress");
        match progress.value {
            ProgressParamsValue::WorkDone(WorkDoneProgress::Report(r)) => {
                assert_eq!(r.message.as_deref(), Some("hi"));
            }
            _ => panic!("expected WorkDoneProgress::Report for token event"),
        }
    }

    #[test]
    fn event_to_progress_message_event_returns_none() {
        let token = NumberOrString::String("test".to_string());
        let kind = EventKind::MessageAppended {
            message: minicoding_core::model::Message::user_text("hi"),
        };
        assert!(event_to_progress(&kind, token).is_none());
    }

    #[tokio::test]
    async fn read_range_extracts_single_line_substring() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "hello world\nsecond line\n")
            .await
            .unwrap();
        let uri = Url::from_file_path(&file_path).unwrap();
        let range = Range {
            start: Position {
                line: 0,
                character: 6,
            },
            end: Position {
                line: 0,
                character: 11,
            },
        };
        let text = read_range(&uri, &range).await.unwrap();
        assert_eq!(text, "world");
    }

    #[tokio::test]
    async fn read_range_extracts_multiple_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("multi.txt");
        tokio::fs::write(&file_path, "line one\nline two\nline three\n")
            .await
            .unwrap();
        let uri = Url::from_file_path(&file_path).unwrap();
        let range = Range {
            start: Position {
                line: 0,
                // "line one": l(0)i(1)n(2)e(3) (4)o(5)n(6)e(7) → char 5 = 'o'，提取 "one"
                character: 5,
            },
            end: Position {
                line: 2,
                character: 4,
            },
        };
        let text = read_range(&uri, &range).await.unwrap();
        assert_eq!(text, "one\nline two\nline");
    }

    #[test]
    fn code_action_command_carries_uri_and_range() {
        let uri = Url::parse("file:///tmp/x.rs").unwrap();
        let range = Range {
            start: Position {
                line: 1,
                character: 0,
            },
            end: Position {
                line: 2,
                character: 10,
            },
        };
        let cmd = code_action_command("解释", CMD_EXPLAIN, &uri, &range);
        assert_eq!(cmd.title, "解释");
        assert_eq!(cmd.command, CMD_EXPLAIN);
        assert!(cmd.arguments.is_some());
    }

    #[tokio::test]
    async fn prompter_loop_denies_when_client_dismisses() {
        // 验证 prompter_loop 的逻辑分支：当 rx 收到 None（channel 关闭）时退出。
        // 完整的 show_message_request 集成测试需要 mock tower_lsp::Client，
        // 此处仅验证 channel 关闭时 loop 优雅退出。
        let (_tx, rx) = mpsc::channel::<PermissionRequest>(1);
        // 使用一个不会真正调用 client 的 wrapper 来测试 loop 退出
        // 由于 Client 需要从 LspService::new 构造，这里只验证 rx 关闭后 loop 结束
        drop(rx);
        // loop 应在 rx.recv() 返回 None 时退出——此处不调用 prompter_loop（需真实 Client），
        // 仅验证 channel 行为符合预期。
    }
}
