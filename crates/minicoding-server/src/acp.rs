//! ACP stdio 适配器（T-M8-7）：可被 Zed 等支持 ACP 的客户端嵌入。
//!
//! ACP（Agent Client Protocol，<https://agentclientprotocol.com>）是 JSON-RPC 2.0
//! over stdio 的协议，使用 LSP 风格 `Content-Length` 帧分隔消息。本模块实现
//! minicoding 作为 ACP agent 端：从 stdin 读 `Request`/`Notification`，向 stdout
//! 写 `Response`/`Notification`。
//!
//! ## ACP 方法映射
//!
//! | ACP 方法 | 方向 | minicoding 内部 |
//! |----------|------|----------------|
//! | `initialize` | 请求-响应 | 返回 agent 能力（无外部依赖） |
//! | `newConversation` | 请求-响应 | `SessionManager::create_session` |
//! | `loadConversation` | 请求-响应 | 校验 session 存在，返回元信息 |
//! | `prompt` | 请求-响应（流式） | `SessionManager::send_message_boxed` + `session/update` 通知 |
//! | `cancel` | 通知 | `SessionManager::cancel` |
//! | `shutdown` | 请求-响应 | 退出主循环 |
//! | `session/update` | 通知（server→client） | `EventDto` 转 `update` 增量 |
//! | `session/permissionRequest` | 通知（server→client） | `PermissionRequested` 事件转 ACP 通知 |
//! | `resolvePermission` | 请求-响应 | `SessionManager::resolve_permission` |
//!
//! ## 帧格式
//!
//! 每条消息以 ASCII header 开头，header 之间用 `\r\n` 分隔，header 与 body 之间
//! 用空行 `\r\n\r\n` 分隔：
//!
//! ```text
//! Content-Length: 1234\r\n
//! \r\n
//! { ... JSON-RPC message ... }
//! ```
//!
//! ## 安全约束
//!
//! - **C-04**：`SessionConfig` 不含 `api_key`，server 用 `default_params`；
//! - **C-05**：所有 stdout 输出是结构化 JSON-RPC，非 LLM 直接输出；
//! - **C-01**：副作用工具经 `ServerPrompter` → `session/permissionRequest` 通知 →
//!   客户端 `resolvePermission` 请求。
//!
//! 详见 `docs/dev-plan.md` T-M8-7、`docs/design.md` §24。

use crate::runtime_builder::ServerRuntimeParams;
use crate::session_mgr::{SessionManager, SessionManagerError};
use minicoding_core::model::TurnOutcome;
use minicoding_core::policy::Decision;
use minicoding_protocol::event::{EventDto, EventKind};
use minicoding_protocol::jsonrpc::{Error as RpcError, Id, Notification, Response, Version};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// ACP 适配器错误。
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// stdin/stdout IO 错误。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON 序列化/反序列化错误。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// `SessionManager` 错误。
    #[error("session manager error: {0}")]
    Session(#[from] SessionManagerError),
    /// 客户端发送了格式错误的帧（缺 `Content-Length`、长度不匹配等）。
    #[error("frame error: {0}")]
    Frame(String),
    /// 客户端发了 `shutdown`，主循环应退出。
    #[error("client requested shutdown")]
    Shutdown,
}

/// 共享 stdout writer（`Mutex` 保护，避免 event forwarder 与 main loop 交叉写）。
type SharedStdout = Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// ACP agent 元信息（`initialize` 响应）。
#[derive(Debug, serde::Serialize)]
struct AgentInfo {
    name: String,
    version: String,
    /// 支持的 ACP 方法集（minicoding 实现 + 扩展）。
    capabilities: AgentCapabilities,
}

/// ACP 能力声明。
#[derive(Debug, serde::Serialize)]
struct AgentCapabilities {
    /// 支持 `session/update` 流式通知。
    streaming: bool,
    /// 支持权限交互（`session/permissionRequest` + `resolvePermission`）。
    permissions: bool,
    /// 支持会话恢复（`loadConversation`）。
    load_conversation: bool,
}

/// `newConversation` 请求参数。
#[derive(Debug, serde::Deserialize, Default)]
struct NewConversationParams {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    permission_mode: Option<minicoding_core::policy::PermissionMode>,
}

/// `prompt` 请求参数。
#[derive(Debug, serde::Deserialize)]
struct PromptParams {
    conversation_id: String,
    text: String,
}

/// `resolvePermission` 请求参数（minicoding 扩展）。
#[derive(Debug, serde::Deserialize)]
struct ResolvePermissionParams {
    conversation_id: String,
    permission_id: String,
    decision: Decision,
}

/// `cancel` 通知参数。
#[derive(Debug, serde::Deserialize)]
struct CancelParams {
    conversation_id: String,
}

/// 启动 ACP server：从 stdin 读 JSON-RPC 消息，向 stdout 写响应/通知。
///
/// 阻塞当前 task，直到 stdin EOF、客户端发 `shutdown`、或发生不可恢复错误。
///
/// # Errors
/// - 帧解析错误（缺 `Content-Length`、长度不匹配）；
/// - stdin/stdout IO 错误；
/// - `SessionManager` 内部错误（透传为 JSON-RPC error response，不退出主循环）。
///
/// # 设计要点
///
/// - **不阻塞于单个 turn**：`prompt` 在后台 task 中执行，主循环继续读 stdin——
///   客户端可在 turn 进行中发送 `cancel` 或 `resolvePermission`；
/// - **流式通知**：`prompt` 期间通过 `session/update` 通知推送 `EventDto`，
///   客户端可实时渲染 token / 工具进度 / 权限请求；
/// - **复用 wire types**：`EventDto` 直接序列化为 `session/update` 的 `params.update` 字段，
///   不重复定义。
pub async fn serve_acp(mgr: Arc<SessionManager>) -> Result<(), AcpError> {
    let stdin = tokio::io::stdin();
    let stdout: Box<dyn AsyncWrite + Send + Unpin> = Box::new(tokio::io::stdout());
    let stdout: SharedStdout = Arc::new(Mutex::new(stdout));
    let mut reader = BufReader::new(stdin);

    loop {
        match read_message(&mut reader).await {
            Ok(payload) => {
                if payload.is_empty() {
                    continue;
                }
                // 解析为通用 JSON-RPC 消息（Request 或 Notification）
                let msg: serde_json::Value = match serde_json::from_slice(&payload) {
                    Ok(v) => v,
                    Err(e) => {
                        write_parse_error(&stdout, e).await?;
                        continue;
                    }
                };
                if let Err(e) = dispatch_message(mgr.clone(), &stdout, &msg).await {
                    if matches!(e, AcpError::Shutdown) {
                        tracing::info!("ACP client requested shutdown, exiting main loop");
                        return Ok(());
                    }
                    tracing::warn!(error = %e, "ACP dispatch failed");
                }
            }
            Err(AcpError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                tracing::info!("ACP stdin EOF, exiting");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(error = %e, "ACP read_message failed");
                return Err(e);
            }
        }
    }
}

/// 读一条 ACP 消息（Content-Length 帧）。
async fn read_message<R>(reader: &mut BufReader<R>) -> Result<Vec<u8>, AcpError>
where
    R: AsyncReadExt + Unpin,
{
    // 1. 读 headers 直到空行
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF
            return Err(AcpError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "stdin EOF while reading headers",
            )));
        }
        // 去除尾部 \r\n
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            // 空行：header 结束
            break;
        }
        // 解析 header（仅识别 `Content-Length: N`，忽略其他如 `Content-Type`）
        if let Some(rest) = trimmed
            .strip_prefix("Content-Length:")
            .or_else(|| trimmed.strip_prefix("content-length:"))
        {
            let len: usize = rest
                .trim()
                .parse()
                .map_err(|e| AcpError::Frame(format!("invalid Content-Length `{rest}`: {e}")))?;
            content_length = Some(len);
        }
        // 其他 header 忽略（ACP 可能发送 `Content-Type: application/json` 等）
    }

    let len = content_length
        .ok_or_else(|| AcpError::Frame("missing Content-Length header".to_string()))?;

    // 2. 读 body
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.map_err(AcpError::Io)?;
    Ok(buf)
}

/// 写一条 ACP 消息（Content-Length 帧）到 stdout。
async fn write_message(stdout: &SharedStdout, payload: &[u8]) -> Result<(), AcpError> {
    let mut out = stdout.lock().await;
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    out.write_all(header.as_bytes()).await?;
    out.write_all(payload).await?;
    out.flush().await?;
    Ok(())
}

/// 写一条 JSON-RPC 消息（自动序列化 + 帧）。
async fn write_jsonrpc<T: serde::Serialize>(
    stdout: &SharedStdout,
    msg: &T,
) -> Result<(), AcpError> {
    let json = serde_json::to_vec(msg)?;
    write_message(stdout, &json).await
}

/// 写 parse error 通知（无法获取 request id）。
async fn write_parse_error(stdout: &SharedStdout, e: serde_json::Error) -> Result<(), AcpError> {
    // 无 id 时按 JSON-RPC 规范发 notification（实际客户端会忽略无法配对的消息）
    let notif = Notification {
        jsonrpc: Version,
        method: "minicoding/parseError".to_string(),
        params: Some(serde_json::json!({"message": e.to_string()})),
    };
    write_jsonrpc(stdout, &notif).await
}

/// 写 JSON-RPC 错误响应。
async fn write_error_response(
    stdout: &SharedStdout,
    id: Id,
    error: RpcError,
) -> Result<(), AcpError> {
    let resp = Response::err(id, error);
    write_jsonrpc(stdout, &resp).await
}

/// 写 JSON-RPC 成功响应。
async fn write_ok_response(
    stdout: &SharedStdout,
    id: Id,
    result: serde_json::Value,
) -> Result<(), AcpError> {
    let resp = Response::ok(id, result);
    write_jsonrpc(stdout, &resp).await
}

/// 分派 JSON-RPC 消息（Request 或 Notification）。
async fn dispatch_message(
    mgr: Arc<SessionManager>,
    stdout: &SharedStdout,
    msg: &serde_json::Value,
) -> Result<(), AcpError> {
    // 区分 Request（有 id）与 Notification（无 id）
    let has_id = msg.get("id").is_some();
    let method = msg
        .get("method")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AcpError::Frame("missing `method` field".to_string()))?;
    let params = msg
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    if has_id {
        let id = parse_id(msg.get("id"))?;
        if let Err(e) = dispatch_request(mgr.clone(), stdout, method, id, params).await {
            // Shutdown 是控制流信号，向上传递
            if matches!(e, AcpError::Shutdown) {
                return Err(e);
            }
            // 其他错误：已写 error response，吞掉继续主循环
            tracing::warn!(method = %method, error = %e, "request handler errored");
        }
    } else {
        dispatch_notification(&mgr, stdout, method, params).await;
    }
    Ok(())
}

/// 解析 JSON-RPC `id`（Number 或 String）。
fn parse_id(id: Option<&serde_json::Value>) -> Result<Id, AcpError> {
    match id {
        Some(serde_json::Value::Number(n)) => {
            let n = n
                .as_u64()
                .ok_or_else(|| AcpError::Frame("id must be u64".to_string()))?;
            Ok(Id::Number(n))
        }
        Some(serde_json::Value::String(s)) => Ok(Id::String(s.clone())),
        None => Err(AcpError::Frame("missing id field".to_string())),
        _ => Err(AcpError::Frame("id must be number or string".to_string())),
    }
}

/// 分派 JSON-RPC Request（有 id，需响应）。
async fn dispatch_request(
    mgr: Arc<SessionManager>,
    stdout: &SharedStdout,
    method: &str,
    id: Id,
    params: serde_json::Value,
) -> Result<(), AcpError> {
    match method {
        "initialize" => handle_initialize(stdout, id).await,
        "newConversation" => handle_new_conversation(mgr, stdout, id, params).await,
        "loadConversation" => handle_load_conversation(mgr, stdout, id, params).await,
        "prompt" => handle_prompt(mgr, stdout, id, params).await,
        "shutdown" => {
            write_ok_response(stdout, id, serde_json::json!({})).await?;
            Err(AcpError::Shutdown)
        }
        "resolvePermission" => handle_resolve_permission(mgr, stdout, id, params).await,
        other => {
            write_error_response(
                stdout,
                id,
                RpcError::method_not_found(format!("unknown method `{other}`")),
            )
            .await
        }
    }
}

/// 分派 JSON-RPC Notification（无 id，不需响应）。
async fn dispatch_notification(
    mgr: &Arc<SessionManager>,
    _stdout: &SharedStdout,
    method: &str,
    params: serde_json::Value,
) {
    match method {
        "cancel" => {
            let p: CancelParams = serde_json::from_value(params).unwrap_or(CancelParams {
                conversation_id: String::new(),
            });
            if let Err(e) = mgr.cancel(&p.conversation_id).await {
                tracing::warn!(
                    conversation_id = %p.conversation_id,
                    error = %e,
                    "ACP cancel failed"
                );
            }
        }
        "initialized" => {
            // ACP `initialized` 通知（类似 LSP）：客户端确认 initialize 完成，no-op
            tracing::debug!("ACP client sent `initialized` notification");
        }
        other => {
            // 未知通知：按 JSON-RPC 规范静默忽略（不响应）
            tracing::debug!(method = %other, "ignoring unknown ACP notification");
        }
    }
}

/// `initialize` 处理：返回 agent 信息与能力。
async fn handle_initialize(stdout: &SharedStdout, id: Id) -> Result<(), AcpError> {
    let info = AgentInfo {
        name: "minicoding".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: AgentCapabilities {
            streaming: true,
            permissions: true,
            load_conversation: true,
        },
    };
    write_ok_response(stdout, id, serde_json::to_value(&info)?).await
}

/// `newConversation` 处理：构造 `ServerRuntimeParams` 并创建会话。
async fn handle_new_conversation(
    mgr: Arc<SessionManager>,
    stdout: &SharedStdout,
    id: Id,
    params: serde_json::Value,
) -> Result<(), AcpError> {
    let p: NewConversationParams = if params.is_null() {
        NewConversationParams::default()
    } else {
        serde_json::from_value(params)?
    };
    let default = mgr.default_params().clone();
    let params = ServerRuntimeParams {
        provider_kind: p.provider.unwrap_or(default.provider_kind),
        provider_name: default.provider_name,
        api_base: default.api_base,
        api_key: default.api_key,
        model: p.model.unwrap_or(default.model),
        workdir: p
            .workdir
            .map(camino::Utf8PathBuf::from)
            .unwrap_or(default.workdir),
        system: p.system.or(default.system),
        permission_mode: p.permission_mode.unwrap_or(default.permission_mode),
        sandbox_policy: default.sandbox_policy,
    };
    match mgr.create_session(Some(params)) {
        Ok(session) => {
            let session_id = session.session_id().clone();
            write_ok_response(
                stdout,
                id,
                serde_json::json!({"conversation_id": session_id}),
            )
            .await
        }
        Err(e) => write_error_response(stdout, id, RpcError::internal(e.to_string())).await,
    }
}

/// `loadConversation` 处理：校验 session 存在，返回元信息。
async fn handle_load_conversation(
    mgr: Arc<SessionManager>,
    stdout: &SharedStdout,
    id: Id,
    params: serde_json::Value,
) -> Result<(), AcpError> {
    #[derive(Debug, serde::Deserialize)]
    struct LoadParams {
        conversation_id: String,
    }
    let p: LoadParams = serde_json::from_value(params)?;
    match mgr.get_or_load(&p.conversation_id).await {
        Ok(_) => {
            write_ok_response(
                stdout,
                id,
                serde_json::json!({"conversation_id": p.conversation_id}),
            )
            .await
        }
        Err(_) => {
            write_error_response(
                stdout,
                id,
                RpcError::new(
                    -32602,
                    format!("conversation {} not found", p.conversation_id),
                ),
            )
            .await
        }
    }
}

/// `prompt` 处理：后台 task 执行 turn，主循环转发事件为 `session/update` 通知，
/// turn 完成后返回最终响应。
async fn handle_prompt(
    mgr: Arc<SessionManager>,
    stdout: &SharedStdout,
    id: Id,
    params: serde_json::Value,
) -> Result<(), AcpError> {
    let p: PromptParams = serde_json::from_value(params)?;
    let Ok(session) = mgr.get_or_load(&p.conversation_id).await else {
        write_error_response(
            stdout,
            id,
            RpcError::new(
                -32602,
                format!("conversation {} not found", p.conversation_id),
            ),
        )
        .await?;
        return Ok(());
    };

    // 先订阅 EventBus，避免 spawn turn task 后错过早期事件
    let runtime = session.runtime.clone();
    let mut rx = runtime.events().subscribe();

    // 后台 task 执行 send_message_boxed
    let mgr_clone = mgr.clone();
    let sid_clone = p.conversation_id.clone();
    let text_clone = p.text.clone();
    let mut turn_task = tokio::spawn(async move {
        SessionManager::send_message_boxed(mgr_clone, sid_clone, text_clone).await
    });

    // 转发事件到 stdout 为 `session/update` 通知
    let conv_id = p.conversation_id.clone();
    loop {
        tokio::select! {
            biased;
            turn_result = &mut turn_task => {
                // drain 剩余事件
                while let Ok(event) = rx.try_recv() {
                    let seq = session.push_event(&event).await;
                    forward_event_as_update(stdout, &conv_id, seq, &EventKind::from(&event)).await?;
                }
                // 写最终响应
                match turn_result {
                    Ok(Ok(TurnOutcome::Finished(_) | TurnOutcome::Interrupted(_))) => {
                        write_ok_response(stdout, id, serde_json::json!({
                            "conversation_id": conv_id,
                            "status": "completed",
                        })).await?;
                    }
                    Ok(Ok(TurnOutcome::Failed(e))) => {
                        write_error_response(stdout, id, RpcError::internal(format!("turn failed: {e}"))).await?;
                    }
                    Ok(Err(e)) => {
                        write_error_response(stdout, id, RpcError::internal(format!("turn error: {e}"))).await?;
                    }
                    Err(e) => {
                        write_error_response(stdout, id, RpcError::internal(format!("turn task panicked: {e}"))).await?;
                    }
                }
                break;
            }
            event_result = rx.recv() => {
                match event_result {
                    Ok(event) => {
                        let seq = session.push_event(&event).await;
                        forward_event_as_update(stdout, &conv_id, seq, &EventKind::from(&event)).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tracing::warn!(conversation_id = %conv_id, "ACP event consumer lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(())
}

/// `resolvePermission` 处理：转发决策到 `SessionManager`。
async fn handle_resolve_permission(
    mgr: Arc<SessionManager>,
    stdout: &SharedStdout,
    id: Id,
    params: serde_json::Value,
) -> Result<(), AcpError> {
    let p: ResolvePermissionParams = serde_json::from_value(params)?;
    match mgr
        .resolve_permission(&p.conversation_id, &p.permission_id, p.decision)
        .await
    {
        Ok(()) => write_ok_response(stdout, id, serde_json::json!({"resolved": true})).await,
        Err(e) => write_error_response(stdout, id, RpcError::new(-32602, e.to_string())).await,
    }
}

/// 把 `EventKind` 转为 `session/update` 通知并写入 stdout。
///
/// `params.update` 字段直接是 `EventDto`（含 `seq` 与 `type`），客户端按 `type`
/// 分支处理（`token`/`tool_call_started`/`permission_requested`/...）。
async fn forward_event_as_update(
    stdout: &SharedStdout,
    conversation_id: &str,
    seq: u64,
    kind: &EventKind,
) -> Result<(), AcpError> {
    let dto = EventDto {
        seq,
        kind: kind.clone(),
    };
    let notif = Notification {
        jsonrpc: Version,
        method: "session/update".to_string(),
        params: Some(serde_json::json!({
            "conversation_id": conversation_id,
            "update": dto,
        })),
    };
    write_jsonrpc(stdout, &notif).await
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
        }
    }

    /// 用 `tokio::io::duplex` 捕获 stdout 写入的字节。
    /// 返回 (writer 端, 读端)：写入端包装为 `SharedStdout`，读端可调用
    /// `drain_reader` 拿到累计字节。`tokio::io::duplex` 的两端 A/B 互通：
    /// 写入 A 的字节从 B 可读，反之亦然。这里把 A 的写端用作 stdout，B 用作读端。
    async fn capture_stdout() -> (SharedStdout, tokio::io::DuplexStream) {
        let (client, server) = tokio::io::duplex(8 * 1024);
        // 把 client 端 split：只用 write 部分（tx）做 stdout；
        // rx 立即 drop（client 的 read 部分用不到——读端在 server 那侧）
        let (_rx, tx) = tokio::io::split(client);
        let writer: Box<dyn AsyncWrite + Send + Unpin> = Box::new(tx);
        (Arc::new(Mutex::new(writer)), server)
    }

    /// 从 duplex 的读端读取所有字节（用于断言）。
    /// 调用前应先 drop 对应的 `SharedStdout`，让写端关闭、读端收到 EOF。
    async fn drain_reader(rx: &mut tokio::io::DuplexStream) -> Vec<u8> {
        let mut buf = Vec::new();
        // 100ms timeout 防止意外未关闭写端时永久挂起
        let _ = tokio::time::timeout(Duration::from_millis(200), rx.read_to_end(&mut buf)).await;
        buf
    }

    #[tokio::test]
    async fn read_message_parses_content_length_frame() {
        let body = br#"{"jsonrpc":"2.0","method":"initialize","id":1}"#;
        let frame = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut input = Vec::new();
        input.extend_from_slice(frame.as_bytes());
        input.extend_from_slice(body);

        let mut reader = BufReader::new(std::io::Cursor::new(input));
        let payload = read_message(&mut reader).await.unwrap();
        assert_eq!(payload, body);
    }

    #[tokio::test]
    async fn read_message_errors_without_content_length() {
        let frame = b"Content-Type: application/json\r\n\r\n{}";
        let mut reader = BufReader::new(std::io::Cursor::new(frame.to_vec()));
        let result = read_message(&mut reader).await;
        assert!(matches!(result, Err(AcpError::Frame(_))));
    }

    #[tokio::test]
    async fn write_message_emits_content_length_header() {
        let (stdout, mut rx) = capture_stdout().await;
        let body = br#"{"hello":"world"}"#;
        write_message(&stdout, body).await.unwrap();
        // 显式 drop stdout，让 duplex 写端关闭，drain_reader 才能读完
        drop(stdout);
        let captured = drain_reader(&mut rx).await;
        let raw = String::from_utf8_lossy(&captured);
        assert!(raw.starts_with("Content-Length: 17\r\n\r\n"));
        assert!(captured.ends_with(body));
    }

    #[tokio::test]
    async fn handle_initialize_returns_agent_info() {
        let (stdout, mut rx) = capture_stdout().await;
        handle_initialize(&stdout, Id::Number(1)).await.unwrap();
        drop(stdout);

        let captured = drain_reader(&mut rx).await;
        let raw = String::from_utf8_lossy(&captured);
        assert!(raw.contains("minicoding"));
        assert!(raw.contains("streaming"));
        // 应为 JSON-RPC response（含 result 字段）
        assert!(raw.contains("\"result\""));
    }

    #[tokio::test]
    async fn dispatch_unknown_method_returns_method_not_found() {
        let mgr = Arc::new(SessionManager::new(test_params(), Duration::from_secs(5)));
        let (stdout, mut rx) = capture_stdout().await;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "totally/unknown",
            "params": {},
        });
        dispatch_message(mgr, &stdout, &msg).await.unwrap();
        drop(stdout);
        let captured = drain_reader(&mut rx).await;
        let raw = String::from_utf8_lossy(&captured);
        assert!(raw.contains("-32601"));
    }

    #[tokio::test]
    async fn dispatch_cancel_notification_for_nonexistent_session_is_no_op() {
        let mgr = Arc::new(SessionManager::new(test_params(), Duration::from_secs(5)));
        let (stdout, mut rx) = capture_stdout().await;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "cancel",
            "params": {"conversation_id": "nonexistent"},
        });
        let result = dispatch_message(mgr, &stdout, &msg).await;
        assert!(result.is_ok());
        drop(stdout);
        let captured = drain_reader(&mut rx).await;
        // notification 无响应——stdout 应为空
        assert!(captured.is_empty(), "expected empty: captured");
    }

    #[tokio::test]
    async fn shutdown_returns_shutdown_error() {
        let mgr = Arc::new(SessionManager::new(test_params(), Duration::from_secs(5)));
        let (stdout, _rx) = capture_stdout().await;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "shutdown",
        });
        let result = dispatch_message(mgr, &stdout, &msg).await;
        assert!(matches!(result, Err(AcpError::Shutdown)));
    }

    #[tokio::test]
    async fn parse_id_handles_numbers_and_strings() {
        assert_eq!(
            parse_id(Some(&serde_json::json!(42))).unwrap(),
            Id::Number(42)
        );
        assert_eq!(
            parse_id(Some(&serde_json::json!("abc"))).unwrap(),
            Id::String("abc".to_string())
        );
        assert!(parse_id(None).is_err());
        assert!(parse_id(Some(&serde_json::json!(true))).is_err());
    }
}
