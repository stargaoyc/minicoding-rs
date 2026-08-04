//! HTTP/SSE 路由与 handler（T-M8-2）。
//!
//! 基于 `axum` 0.8 实现 REST 风格 HTTP 接口，body 用 JSON（`serde_json::Value`）。
//! 路由设计见 `docs/design.md` §24：
//!
//! ```text
//! POST   /sessions                          → CreateSession
//! POST   /sessions/{id}/messages            → SendUserMessage（阻塞至 turn 完成）
//! POST   /sessions/{id}/cancel              → Cancel
//! GET    /sessions                          → ListSessions
//! GET    /sessions/{id}                     → GetSession
//! GET    /sessions/{id}/events              → SSE 事件流（Last-Event-ID 恢复）
//! POST   /sessions/{id}/permissions/{pid}   → ResolvePermission
//! ```
//!
//! `Undo` 端点暂未实现（需 `Journal` feature gate，T-M8 后续补）。

use crate::runtime_builder::ServerRuntimeParams;
use crate::session_mgr::{SessionManager, SessionManagerError};
use crate::sse;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use camino::Utf8PathBuf;
use minicoding_core::model::TurnOutcome;
use minicoding_core::policy::{Decision, PermissionMode};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

/// Server 配置（`serve` 子命令传入）。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 监听地址（如 `127.0.0.1:8080`）。
    pub bind: SocketAddr,
    /// 默认 LLM provider 类型（`openai`/`anthropic`/`ollama`）。
    pub provider_kind: String,
    /// 默认 provider 自定义显示名（`None` 回退到 `provider_kind`，与 CLI `--provider-name` 对齐）。
    pub provider_name: Option<String>,
    /// 默认 API base URL。
    pub api_base: String,
    /// 默认 API key（Ollama 可为空）。
    pub api_key: String,
    /// 默认模型名称。
    pub model: String,
    /// 默认工作目录。
    pub workdir: Utf8PathBuf,
    /// 默认系统 prompt。
    pub system: Option<String>,
    /// 权限交互超时（秒，默认 300）。
    pub permission_timeout_sec: u64,
    /// 静态资源目录（M9 `--web`，见 `design.md` §26.7）。
    ///
    /// 设为 `Some(dir)` 时，HTTP server 用 `ServeDir` 托管该目录下的静态文件
    /// （前端 SPA），`GET /` 返回 `index.html`，未匹配的路径 fallback 到
    /// `index.html`（SPA history 路由）。设为 `None` 时仅暴露 API/SSE 路由。
    pub web_dir: Option<Utf8PathBuf>,
    /// CORS 允许的来源列表（M9 `--cors-origin`，见 `design.md` §26.6）。
    ///
    /// 空列表 = 允许任意来源（`Access-Control-Allow-Origin: *`，开发默认）；
    /// 非空列表 = 仅允许列出的来源（精确匹配，生产部署用）。桌面模式同源无需配置。
    pub cors_origins: Vec<String>,
}

/// 构造默认 `ServerRuntimeParams`（从 `ServerConfig` 派生）。
fn default_params(cfg: &ServerConfig) -> ServerRuntimeParams {
    ServerRuntimeParams {
        provider_kind: cfg.provider_kind.clone(),
        provider_name: cfg.provider_name.clone(),
        api_base: cfg.api_base.clone(),
        api_key: cfg.api_key.clone(),
        model: cfg.model.clone(),
        workdir: cfg.workdir.clone(),
        system: cfg.system.clone(),
        permission_mode: PermissionMode::Default,
    }
}

/// 共享状态（注入到 axum `State`）。
#[derive(Clone)]
pub struct AppState {
    pub mgr: Arc<SessionManager>,
}

/// `CreateSession` 请求 body。
#[derive(Debug, Deserialize, Default)]
struct CreateSessionBody {
    #[serde(default)]
    provider: Option<String>,
    /// 自定义 provider 显示名（覆盖 server 默认，`None` 用 `default_params.provider_name`）。
    #[serde(default)]
    provider_name: Option<String>,
    #[serde(default)]
    api_base: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    permission_mode: Option<PermissionMode>,
}

/// `CreateSession` 响应。
#[derive(Debug, Serialize)]
struct CreateSessionResponse {
    session_id: String,
}

/// `SendUserMessage` 请求 body。
#[derive(Debug, Deserialize)]
struct SendMessageBody {
    text: String,
}

/// `SendUserMessage` 响应。
#[derive(Debug, Serialize)]
struct SendMessageResponse {
    stop_reason: String,
    final_text: String,
}

/// `ResolvePermission` 请求 body。
#[derive(Debug, Deserialize)]
struct ResolvePermissionBody {
    decision: Decision,
}

/// `GetSession` 响应。
#[derive(Debug, Serialize)]
struct GetSessionResponse {
    session_id: String,
    messages: Vec<minicoding_core::model::Message>,
}

/// `ListSessions` 响应。
#[derive(Debug, Serialize)]
struct ListSessionsResponse {
    sessions: Vec<minicoding_core::model::SessionMeta>,
}

/// HTTP 错误响应（实现 `IntoResponse`，handler 用 `Result<T, HttpError>` 返回）。
#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    message: String,
}

impl IntoResponse for HttpError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(serde_json::json!({"error": self.message})),
        )
            .into_response()
    }
}

impl From<SessionManagerError> for HttpError {
    fn from(e: SessionManagerError) -> Self {
        let status = match &e {
            SessionManagerError::NotFound(_) | SessionManagerError::PermissionNotFound(_) => {
                StatusCode::NOT_FOUND
            }
            SessionManagerError::AlreadyExists(_) | SessionManagerError::TurnInProgress(_) => {
                StatusCode::CONFLICT
            }
            SessionManagerError::BuildFailed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: e.to_string(),
        }
    }
}

/// 构造 axum Router。
///
/// `web_dir` 设为 `Some(dir)` 时在 API/SSE 路由之外挂载 `ServeDir`（M9 `--web`，
/// 见 `design.md` §26.7），未匹配静态文件的路径 fallback 到 `index.html`（SPA
/// history 路由）。
///
/// `cors_origins` 空列表 = 允许任意来源（开发默认）；非空 = 仅允许列出的来源
/// （生产部署，M9 `--cors-origin`，见 `design.md` §26.6）。
fn build_router(state: AppState, web_dir: Option<&Utf8PathBuf>, cors_origins: &[String]) -> Router {
    // CORS：空列表用 Any（开发默认），非空列表精确匹配（生产部署）
    let cors = if cors_origins.is_empty() {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        let origins: Vec<_> = cors_origins
            .iter()
            .filter_map(|s| s.parse::<axum::http::HeaderValue>().ok())
            .collect();
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods(Any)
            .allow_headers(Any)
    };

    let api_routes = Router::new()
        .route("/sessions", post(create_session).get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/messages", post(send_message))
        .route("/sessions/{id}/cancel", post(cancel_turn))
        .route("/sessions/{id}/events", get(sse_events))
        .route("/sessions/{id}/permissions/{pid}", post(resolve_permission))
        .route("/health", get(health));

    let app = if let Some(dir) = web_dir {
        // M9：静态资源托管（SPA）。API 路由优先匹配，未命中的路径走 `fallback_service`
        // 到 `ServeDir`，再由 ServeDir 的 `fallback` 回退到 `ServeFile(index.html)`（SPA
        // history 路由）。axum 0.8 不再支持 `nest_service("/")`，改用 `fallback_service`。
        let serve_dir = ServeDir::new(dir).fallback(ServeFile::new(dir.join("index.html")));
        Router::new().merge(api_routes).fallback_service(serve_dir)
    } else {
        Router::new().merge(api_routes)
    };

    app.layer(cors).with_state(state)
}

/// 启动 HTTP server（阻塞当前 task）。
///
/// # Errors
/// Server bind 失败或运行时错误时返回 `anyhow::Error`。
pub async fn serve(cfg: ServerConfig) -> anyhow::Result<()> {
    let params = default_params(&cfg);
    let permission_timeout = Duration::from_secs(cfg.permission_timeout_sec);
    let mgr = Arc::new(SessionManager::new(params, permission_timeout));
    let state = AppState { mgr };

    let app = build_router(state, cfg.web_dir.as_ref(), &cfg.cors_origins);
    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .map_err(|e| anyhow::anyhow!("bind {addr} 失败: {e}", addr = cfg.bind))?;
    tracing::info!(addr = %cfg.bind, web_dir = ?cfg.web_dir, "minicoding-server 启动");
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("server 运行错误: {e}"))?;
    Ok(())
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// `GET /health` — 健康检查。
async fn health() -> &'static str {
    "ok"
}

/// `POST /sessions` — 创建新会话。
async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionBody>,
) -> Result<Json<CreateSessionResponse>, HttpError> {
    let default = state.mgr.default_params();
    let params = ServerRuntimeParams {
        provider_kind: body
            .provider
            .unwrap_or_else(|| default.provider_kind.clone()),
        provider_name: body.provider_name.or(default.provider_name.clone()),
        api_base: body.api_base.unwrap_or_else(|| default.api_base.clone()),
        api_key: body.api_key.unwrap_or_else(|| default.api_key.clone()),
        model: body.model.unwrap_or_else(|| default.model.clone()),
        workdir: body
            .workdir
            .map_or_else(|| default.workdir.clone(), Utf8PathBuf::from),
        system: body.system.or(default.system.clone()),
        permission_mode: body.permission_mode.unwrap_or(default.permission_mode),
    };

    let session = state.mgr.create_session(Some(params))?;
    let session_id = session.session_id().clone();
    Ok(Json(CreateSessionResponse { session_id }))
}

/// `GET /sessions` — 列出所有会话。
async fn list_sessions(State(state): State<AppState>) -> Json<ListSessionsResponse> {
    let sessions = state.mgr.list_sessions();
    Json(ListSessionsResponse { sessions })
}

/// `GET /sessions/{id}` — 获取会话详情（含消息快照）。
async fn get_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<GetSessionResponse>, HttpError> {
    let messages = state.mgr.get_messages(&session_id).await?;
    Ok(Json(GetSessionResponse {
        session_id,
        messages,
    }))
}

/// `POST /sessions/{id}/messages` — 发送用户消息（阻塞至 turn 完成）。
///
/// 调用 `SessionManager::send_message_boxed`（返回 `BoxFuture<'static>`）避免
/// `async fn(&self, ..)` 的 future 借用 `&self` 与 axum `Handler` trait 的 HRTB 冲突。
async fn send_message(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<SendMessageBody>,
) -> Result<Json<SendMessageResponse>, HttpError> {
    let outcome =
        SessionManager::send_message_boxed(state.mgr.clone(), session_id, body.text).await?;
    match outcome {
        TurnOutcome::Finished(msg) => Ok(Json(SendMessageResponse {
            stop_reason: "end_turn".to_string(),
            final_text: msg.text(),
        })),
        TurnOutcome::Interrupted(msg) => Ok(Json(SendMessageResponse {
            stop_reason: "interrupted".to_string(),
            final_text: msg.text(),
        })),
        TurnOutcome::Failed(e) => Err(HttpError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: e.to_string(),
        }),
    }
}

/// `POST /sessions/{id}/cancel` — 取消当前 turn。
async fn cancel_turn(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, HttpError> {
    state.mgr.cancel(&session_id)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// `GET /sessions/{id}/events` — SSE 事件流。
async fn sse_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>>, HttpError> {
    let session = state
        .mgr
        .get(&session_id)
        .ok_or_else(|| SessionManagerError::NotFound(session_id.clone()))?;

    let last_seq =
        sse::parse_last_event_id(headers.get("last-event-id").and_then(|v| v.to_str().ok()));

    let stream = sse::sse_stream(session, last_seq);
    let mapped = stream.map(|item| {
        let sse_str = item.unwrap_or_default();
        Ok::<_, Infallible>(parse_sse_block(&sse_str))
    });

    Ok(Sse::new(mapped).keep_alive(KeepAlive::default()))
}

/// `POST /sessions/{id}/permissions/{pid}` — 解析权限请求。
async fn resolve_permission(
    State(state): State<AppState>,
    Path((session_id, permission_id)): Path<(String, String)>,
    Json(body): Json<ResolvePermissionBody>,
) -> Result<Json<serde_json::Value>, HttpError> {
    state
        .mgr
        .resolve_permission(&session_id, &permission_id, body.decision)
        .await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// 解析预格式化的 SSE 块为 `axum::response::sse::Event`。
fn parse_sse_block(block: &str) -> SseEvent {
    let mut id = None;
    let mut event_name = None;
    let mut data = String::new();

    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("id: ") {
            id = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("event: ") {
            event_name = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("data: ") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        }
    }

    let mut sse_event = SseEvent::default().data(data);
    if let Some(name) = event_name {
        sse_event = sse_event.event(name);
    }
    if let Some(seq) = id {
        sse_event = sse_event.id(seq);
    }
    sse_event
}
