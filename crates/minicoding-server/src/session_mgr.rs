//! `SessionManager`：多会话管理（T-M8-2）。
//!
//! 每个 `ServerSession` 持有一个 `Arc<Runtime>` + `EventCursor` + `PendingPermissions`。
//! HTTP handler 通过 `SessionManager` 创建/查找/删除会话。
//!
//! **并发模型**：每个 session 同一时刻只允许一个 `run_turn` 在执行（`Runtime` 是
//! 单会话聚合根，C-31 上下文一致性）。`SendUserMessage` handler 在调用 `run_turn`
//! 前获取 session 级 `Mutex`，并发请求排队等待（第二个请求在第一个 turn 完成后才开始）。
//!
//! **事件 seq 分配**：`EventCursor` 为每个事件分配单调递增 `seq`，SSE 流用 `seq`
//! 做 cursor 恢复（见 `sse.rs`）。

use crate::prompter::{PendingPermissions, ServerPrompter};
use crate::runtime_builder::{ServerRuntimeParams, build_runtime};
use minicoding_core::model::{Message, SessionId, SessionMeta, TurnOutcome, UserInput};
use minicoding_core::policy::Decision;
use minicoding_core::runtime::{Event, Runtime};
use minicoding_protocol::cursor::EventCursor;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex as TokioMutex;

/// Server 端会话错误。
#[derive(Debug, Error)]
pub enum SessionManagerError {
    /// 会话不存在。
    #[error("session {0} not found")]
    NotFound(SessionId),
    /// 会话已存在（CreateSession 重复 id）。
    #[error("session {0} already exists")]
    AlreadyExists(SessionId),
    /// Runtime 构造失败。
    #[error("runtime build failed: {0}")]
    BuildFailed(String),
    /// 当前已有 turn 在执行（不阻塞模式返回此错误）。
    #[error("session {0} has a turn in progress")]
    TurnInProgress(SessionId),
    /// 权限请求不存在（已 resolved 或超时）。
    #[error("permission {0} not found (already resolved or expired)")]
    PermissionNotFound(String),
}

/// 单个 server 会话的状态。
///
/// 持有 `Arc<Runtime>`（单会话聚合根）、`EventCursor`（seq 分配）、
/// `PendingPermissions`（权限交互表）、`turn_lock`（turn 串行化）。
pub struct ServerSession {
    /// Runtime 聚合根（单会话）。
    pub runtime: Arc<Runtime>,
    /// 事件 seq 分配器（SSE cursor 恢复用）。`TokioMutex` 因 `push_event`/`replay_after`
    /// 在 async 上下文中调用，且 lock 持续时间短。
    pub cursor: TokioMutex<EventCursor>,
    /// pending 权限请求表（`ServerPrompter` 共享）。`TokioMutex` 因 `prompt` 在
    /// `timeout().await` 上下文中持有锁。
    pub pending_permissions: PendingPermissions,
    /// turn 串行锁（同一 session 同一时刻只允许一个 turn）。`TokioMutex` 因锁跨
    /// `run_turn().await` 持有（不能跨 await 持有 `std::sync::Mutex`）。
    pub turn_lock: TokioMutex<()>,
}

impl ServerSession {
    /// 创建新会话状态。
    fn new(runtime: Arc<Runtime>, pending: PendingPermissions) -> Self {
        Self {
            runtime,
            cursor: TokioMutex::new(EventCursor::new(1024)),
            pending_permissions: pending,
            turn_lock: TokioMutex::new(()),
        }
    }

    /// 返回会话 ID。
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.runtime.session().id
    }

    /// 分配 seq 并把事件推入 cursor ring buffer，返回分配的 seq。
    ///
    /// `EventKind` 序列化为 JSON 后存入 `EventCursor`，SSE 流用 `replay_after_with_seq`
    /// 重放，把 seq 填入 SSE `id:` 字段。
    pub async fn push_event(&self, event: &Event) -> u64 {
        let kind = minicoding_protocol::event::EventKind::from(event);
        let json = serde_json::to_value(&kind).unwrap_or(serde_json::Value::Null);
        let mut cursor = self.cursor.lock().await;
        cursor.push(json)
    }

    /// 从 `after_seq` 之后重放事件（SSE 恢复用），返回 `(seq, Value)` 列表。
    ///
    /// `None` 表示 `after_seq` 已 evict 且不可恢复——SSE handler 应发 `RehydrateRequired`。
    pub async fn replay_after(&self, after_seq: u64) -> Option<Vec<(u64, serde_json::Value)>> {
        let cursor = self.cursor.lock().await;
        let replay = cursor.replay_after_with_seq(after_seq)?;
        Some(replay.into_iter().map(|(s, v)| (s, v.clone())).collect())
    }
}

/// 多会话管理器。
pub struct SessionManager {
    /// 活跃会话表。`std::sync::Mutex` 因仅做 `HashMap` 查/增/删（纯数据，非 IO 资源），
    /// 按 tokio 官方建议用阻塞锁——避免 `async fn(&self, &str)` 的 future 借用
    /// `&self` 与 axum Handler HRTB 冲突。
    sessions: std::sync::Mutex<HashMap<SessionId, Arc<ServerSession>>>,
    /// 默认 Runtime 构造参数（从 CLI/env 读取，CreateSession 时使用）。
    default_params: ServerRuntimeParams,
    /// 权限交互超时（默认 300s）。
    permission_timeout: Duration,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("permission_timeout", &self.permission_timeout)
            .finish_non_exhaustive()
    }
}

impl SessionManager {
    /// 创建 `SessionManager`。
    ///
    /// `default_params` 是 `CreateSession` 未指定 provider/model 时的默认参数。
    #[must_use]
    pub fn new(default_params: ServerRuntimeParams, permission_timeout: Duration) -> Self {
        Self {
            sessions: std::sync::Mutex::new(HashMap::new()),
            default_params,
            permission_timeout,
        }
    }

    /// 返回默认 Runtime 构造参数的引用（HTTP `CreateSession` handler 用）。
    #[must_use]
    pub fn default_params(&self) -> &ServerRuntimeParams {
        &self.default_params
    }

    /// 创建新会话。
    ///
    /// `params_override` 为 `Some` 时覆盖默认参数（如客户端指定 provider/model）。
    /// 构造 `ServerPrompter` + `Runtime`，注册到 sessions map。
    ///
    /// `build_runtime` 是同步函数（构造 Runtime 不涉及 await），同步返回。
    ///
    /// # Errors
    /// Runtime 构造失败时返回 `SessionManagerError::BuildFailed`。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic（理论不可达，除非另一线程 panic）。
    pub fn create_session(
        &self,
        params_override: Option<ServerRuntimeParams>,
    ) -> Result<Arc<ServerSession>, SessionManagerError> {
        let params = params_override.unwrap_or_else(|| self.default_params.clone());
        let pending: PendingPermissions = Arc::new(TokioMutex::new(HashMap::new()));
        let prompter: Arc<dyn minicoding_core::policy::PermissionPrompter> = Arc::new(
            ServerPrompter::new(pending.clone(), self.permission_timeout),
        );
        self.insert_session(&params, prompter, pending)
    }

    /// 创建新会话并注入自定义 `PermissionPrompter`（T-M8-9，LSP 端用 `LspPrompter`）。
    ///
    /// 与 `create_session` 的区别：prompter 由调用方提供（如 `LspPrompter`，通过
    /// `window/showMessageRequest` 完成权限交互），不使用 `ServerPrompter`。
    /// `pending_permissions` 仍创建（空 map，LSP 端不经 HTTP resolve 路径），
    /// 保持 `ServerSession` 结构一致。
    ///
    /// # Errors
    /// Runtime 构造失败时返回 `SessionManagerError::BuildFailed`。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic。
    pub fn create_session_with_prompter(
        &self,
        params_override: Option<ServerRuntimeParams>,
        prompter: Arc<dyn minicoding_core::policy::PermissionPrompter>,
    ) -> Result<Arc<ServerSession>, SessionManagerError> {
        let params = params_override.unwrap_or_else(|| self.default_params.clone());
        // LSP 端 `pending_permissions` 不使用（权限交互走 showMessageRequest，
        // 不经 HTTP `resolve_permission` 路径），但 `ServerSession` 结构需要此字段。
        let pending: PendingPermissions = Arc::new(TokioMutex::new(HashMap::new()));
        self.insert_session(&params, prompter, pending)
    }

    /// 内部：构造 Runtime + `ServerSession` 并注册到 sessions map。
    fn insert_session(
        &self,
        params: &ServerRuntimeParams,
        prompter: Arc<dyn minicoding_core::policy::PermissionPrompter>,
        pending: PendingPermissions,
    ) -> Result<Arc<ServerSession>, SessionManagerError> {
        let runtime = build_runtime(params, prompter)
            .map_err(|e| SessionManagerError::BuildFailed(e.to_string()))?;
        let runtime = Arc::new(runtime);
        let session = Arc::new(ServerSession::new(runtime, pending));

        let session_id = session.session_id().clone();
        let mut guard = self.sessions.lock().expect("sessions mutex poisoned");
        guard.insert(session_id, session.clone());
        Ok(session)
    }

    /// 查找会话（同步——`std::sync::Mutex` 的 `HashMap` 查找不需 await）。
    ///
    /// 同步函数消除 `async fn(&self, &str)` 的 future 生命周期参数，
    /// 避免 axum Handler HRTB 冲突。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic。
    #[must_use]
    pub fn get(&self, session_id: &str) -> Option<Arc<ServerSession>> {
        let guard = self.sessions.lock().expect("sessions mutex poisoned");
        guard.get(session_id).cloned()
    }

    /// 列出所有会话（同步——仅读 `HashMap`）。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic。
    pub fn list_sessions(&self) -> Vec<SessionMeta> {
        let guard = self.sessions.lock().expect("sessions mutex poisoned");
        let mut metas = Vec::new();
        for session in guard.values() {
            let runtime = &session.runtime;
            let session_model = runtime.session();
            metas.push(SessionMeta {
                id: session_model.id.clone(),
                created_at: session_model.created_at,
                message_count: session_model.messages.len(),
                last_message_at: session_model
                    .messages
                    .last()
                    .map_or(session_model.created_at, |m| m.created_at),
                tasks: Vec::new(),
            });
        }
        metas
    }

    /// 删除会话（同步——仅从 `HashMap` 移除）。
    ///
    /// # Panics
    /// 内部 `sessions` Mutex poisoned 时 panic。
    pub fn delete(&self, session_id: &str) -> bool {
        let mut guard = self.sessions.lock().expect("sessions mutex poisoned");
        guard.remove(session_id).is_some()
    }

    /// 解析权限请求（HTTP `POST /permissions/{pid}` 调用）。
    ///
    /// 查找 pending map，发送决策到 `ServerPrompter` 的 oneshot channel。
    ///
    /// # Errors
    /// 权限请求不存在（已 resolved 或超时）时返回 `PermissionNotFound`。
    pub async fn resolve_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        decision: Decision,
    ) -> Result<(), SessionManagerError> {
        let session = self
            .get(session_id)
            .ok_or_else(|| SessionManagerError::NotFound(session_id.to_string()))?;
        let mut guard = session.pending_permissions.lock().await;
        match guard.remove(permission_id) {
            Some(tx) => {
                let _ = tx.send(decision);
                Ok(())
            }
            None => Err(SessionManagerError::PermissionNotFound(
                permission_id.to_string(),
            )),
        }
    }

    /// 发送用户消息并驱动 turn（阻塞至 turn 完成）。
    ///
    /// **设计原因**：关联函数（非 `&self` 方法）+ owned `Arc<SessionManager>` 参数，
    /// 返回的 future 无外部借用（`'static`），避免 `async fn(&self, ..)` 与 axum
    /// `Handler` trait HRTB 的冲突。
    ///
    /// 内部获取 `turn_lock`（串行化），订阅 `EventBus` 收集事件分配 seq，
    /// 调用 `Runtime::run_turn_owned`。
    ///
    /// # Errors
    /// - 会话不存在：`NotFound`；
    /// - `run_turn` 失败：透传 `RuntimeError`。
    pub async fn send_message_boxed(
        mgr: Arc<SessionManager>,
        session_id: String,
        text: String,
    ) -> Result<TurnOutcome, SessionManagerError> {
        let session = mgr
            .get(&session_id)
            .ok_or_else(|| SessionManagerError::NotFound(session_id.clone()))?;

        // 获取 turn 锁（串行化：同一 session 同时只有一个 turn）
        let _turn_guard = session.turn_lock.lock().await;

        // Clone `Arc<Runtime>` 断开 `session.runtime` 的 Arc-deref 借用链。
        let runtime = session.runtime.clone();
        let events = runtime.events().clone();

        // 订阅 `EventBus`（在 run_turn 之前订阅，避免错过早期事件）
        let mut rx = events.subscribe();

        // 后台 task：消费 `EventBus` 事件，分配 seq，推入 cursor ring buffer
        let session_clone = session.clone();
        let event_task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        session_clone.push_event(&event).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // 消费慢导致丢事件——SSE 客户端会收到 RehydrateRequired
                        tracing::warn!("SessionManager event consumer lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // 驱动 turn。`runtime` 是 owned `Arc<Runtime>`，`run_turn(&self)` 借用
        // `&*runtime`（局部借用，future 自包含）。
        let user_input = UserInput::from_text(text);
        let result = runtime.run_turn(user_input).await;

        // turn 结束后停止事件消费 task
        event_task.abort();

        match result {
            Ok(outcome) => Ok(outcome),
            Err(e) => Err(SessionManagerError::BuildFailed(e.to_string())),
        }
    }

    /// 取消当前 turn（同步——`Runtime::cancel` 仅触发 `CancellationToken`，无 await）。
    ///
    /// # Errors
    /// 会话不存在时返回 `NotFound`。
    pub fn cancel(&self, session_id: &str) -> Result<(), SessionManagerError> {
        let session = self
            .get(session_id)
            .ok_or_else(|| SessionManagerError::NotFound(session_id.to_string()))?;
        session.runtime.cancel();
        Ok(())
    }

    /// 获取会话消息快照（`GET /sessions/{id}` 用）。
    ///
    /// # Errors
    /// - 会话不存在：`NotFound`；
    /// - `Storage::load` 失败：`BuildFailed`。
    pub async fn get_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, SessionManagerError> {
        let session = self
            .get(session_id)
            .ok_or_else(|| SessionManagerError::NotFound(session_id.to_string()))?;
        // 从 storage 加载完整消息历史
        let storage = session.runtime.storage();
        let messages = storage
            .load(&session.runtime.session().id)
            .await
            .map_err(|e| SessionManagerError::BuildFailed(e.to_string()))?;
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;

    fn test_params() -> ServerRuntimeParams {
        ServerRuntimeParams {
            provider_kind: "openai".to_string(),
            api_base: "http://localhost:8080/v1".to_string(),
            api_key: "sk-test".to_string(),
            model: "gpt-4o".to_string(),
            workdir: Utf8PathBuf::from("."),
            system: None,
            permission_mode: minicoding_core::policy::PermissionMode::Default,
        }
    }

    #[tokio::test]
    async fn resolve_nonexistent_permission_errors() {
        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        // 会话不存在
        let result = mgr
            .resolve_permission("nonexistent", "perm-1", Decision::Allow)
            .await;
        assert!(matches!(result, Err(SessionManagerError::NotFound(_))));
    }

    #[tokio::test]
    async fn get_nonexistent_session_returns_none() {
        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        let result = mgr.get("nonexistent");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_sessions_returns_empty() {
        let mgr = SessionManager::new(test_params(), Duration::from_secs(5));
        let list = mgr.list_sessions();
        assert!(list.is_empty());
    }
}
