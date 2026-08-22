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
//! GET    /sessions/{id}/workspace           → WorkspaceRoot（W-11 工作区）
//! GET    /sessions/{id}/workspace/list      → WorkspaceList（单层，ignore 过滤）
//! GET    /sessions/{id}/workspace/read      → WorkspaceRead（≤ 64 KiB 截断）
//! GET    /sessions/{id}/workspace/diff      → WorkspaceDiff（journal 改动历史）
//! POST   /sessions/{id}/workspace           → WorkspaceSwitch（Ask 审批）
//! ```
//!
//! Workspace 端点见 `design.md` §26.9：只读浏览等价 `fs.read` 不经权限（C-01），
//! 路径经 `resolve_path` 校验（C-03）；切换工作区走 Ask 审批 + 审计（复用
//! W-03 权限弹窗与 `Event::PermissionRequested`）。
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
use minicoding_core::sandbox::SandboxPolicy;
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
    /// 空列表 = 默认仅允许**本机来源**（`localhost`/`127.0.0.1`/`[::1]` 任意端口，S2 防浏览器
    /// drive-by）；非空列表 = 仅允许列出的精确来源。`*` 通配不支持。
    pub cors_origins: Vec<String>,
    /// API 鉴权 token（S1/C-01 配套）。`Some(t)` = 除 `/health` 外全端点强制
    /// `Authorization: Bearer <t>` 或 `?token=<t>`（`EventSource` 专用）；
    /// `None` = 关闭鉴权（调用方须向用户输出红字警告并记审计语义风险）。
    pub auth_token: Option<String>,
    /// 默认安全预设（`auto`/`read-only`/`external-sandbox`/`full-access`）。
    pub preset: String,
    /// LLM 请求超时（秒，默认 120）。
    pub timeout_sec: u64,
    /// LLM 请求最大重试（默认 3，C-13 bounded retries）。
    pub max_retries: u32,
    /// 小 LLM 模型名（摘要/压缩降本，`None` 不启用，见 `design.md` §3.8）。
    pub small_model: Option<String>,
    /// 单 turn 超时（秒，默认 600）。
    pub turn_timeout_sec: u64,
    /// 上下文压缩开关（默认开启，C-18 软约束）。
    pub compress: bool,
}

/// 构造默认 `ServerRuntimeParams`（从 `ServerConfig` 派生）。
fn default_params(cfg: &ServerConfig) -> ServerRuntimeParams {
    let mut params = ServerRuntimeParams {
        provider_kind: cfg.provider_kind.clone(),
        provider_name: cfg.provider_name.clone(),
        api_base: cfg.api_base.clone(),
        api_key: cfg.api_key.clone(),
        model: cfg.model.clone(),
        workdir: cfg.workdir.clone(),
        system: cfg.system.clone(),
        permission_mode: PermissionMode::Default,
        sandbox_policy: SandboxPolicy::WorkspaceWrite {
            workdir: cfg.workdir.clone(),
            writable: Vec::new(),
        },
        timeout_sec: cfg.timeout_sec,
        max_retries: cfg.max_retries,
        small_model: cfg.small_model.clone(),
        turn_timeout_sec: cfg.turn_timeout_sec,
        compress: cfg.compress,
    };
    // `--preset` 覆盖默认模式与沙箱策略（C-22：full-access 打印 red 警告）
    if let Ok((mode, policy, warning)) = build_preset_policy(&cfg.preset, &cfg.workdir) {
        params.permission_mode = mode;
        params.sandbox_policy = policy;
        if let Some(w) = warning {
            eprintln!("\x1b[31m{w}\x1b[0m");
        }
    }
    params
}

/// 共享状态（注入到 axum `State`）。
#[derive(Clone)]
pub struct AppState {
    pub mgr: Arc<SessionManager>,
    /// server 默认配置快照（`GET /config` 返回，供前端设置面板加载真实默认值）。
    pub cfg: ServerConfig,
}

/// `GET /config` 响应（server 当前默认配置，不含 API key，C-04）。
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "../../minicoding-web/src/api/generated/"))]
pub struct ServerConfigResponse {
    provider_kind: String,
    provider_name: Option<String>,
    api_base: String,
    model: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    timeout_sec: u64,
    max_retries: u32,
    small_model: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    turn_timeout_sec: u64,
    compress: bool,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    permission_timeout_sec: u64,
    preset: String,
    /// 配置修订号（M-10 防陈旧写：前端保存前锁定基准，config.toml 实时值）。
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    config_revision: u64,
}

/// `CreateSession` 请求 body。
#[derive(Debug, Deserialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "../../minicoding-web/src/api/generated/"))]
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
    /// 安全预设（`auto`/`read-only`/`external-sandbox`/`full-access`，见 `Preset`）。
    /// `full-access` = 沙箱外全自动运行（仅受信容器内，C-22 red 警告）。
    #[serde(default)]
    preset: Option<String>,
    /// 高危预设（`full-access`/`external-sandbox`）的二次确认字段（S3/C-22）：
    /// UI 弹出红色警告确认后置 true 回传；缺失或 false 时请求被拒。
    #[serde(default)]
    confirm_danger: Option<bool>,
    /// Plan 模式（C-25：先规划后执行，写 `plan.md` + 子任务拆分，仅只读工具可用）。
    /// `true` 时会话初始 `PermissionMode` 为 `Plan`（客户端显式 `body.permission_mode`
    /// 优先于本开关）。
    #[serde(default)]
    plan_mode: bool,
    /// LLM 请求超时（秒，覆盖 server 默认）。
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    timeout_sec: Option<u64>,
    /// LLM 请求最大重试（覆盖 server 默认）。
    #[serde(default)]
    max_retries: Option<u32>,
    /// 小 LLM 模型名（摘要/压缩降本，`None` 继承 server 默认，见 `design.md` §3.8）。
    #[serde(default)]
    small_model: Option<String>,
    /// 单 turn 超时（秒，覆盖 server 默认）。
    #[serde(default)]
    turn_timeout_sec: Option<u64>,
    /// 上下文压缩开关（覆盖 server 默认，C-18 软约束）。
    #[serde(default)]
    compress: Option<bool>,
}

/// `CreateSession` 响应。
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "../../minicoding-web/src/api/generated/"))]
struct CreateSessionResponse {
    session_id: String,
}

/// `SendUserMessage` 请求 body。
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "../../minicoding-web/src/api/generated/"))]
struct SendMessageBody {
    text: String,
}

/// `SendUserMessage` 响应。
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "../../minicoding-web/src/api/generated/"))]
struct SendMessageResponse {
    stop_reason: String,
    final_text: String,
}

/// `ResolvePermission` 请求 body。
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "../../minicoding-web/src/api/generated/"))]
struct ResolvePermissionBody {
    decision: Decision,
}

/// `GetSession` 响应。
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "../../minicoding-web/src/api/generated/"))]
struct GetSessionResponse {
    session_id: String,
    messages: Vec<minicoding_core::model::Message>,
    /// 任务列表快照（`task_state`，见 `SessionManager`）。
    tasks: Vec<minicoding_core::model::Task>,
}

/// `ListSessions` 响应。
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "../../minicoding-web/src/api/generated/"))]
struct ListSessionsResponse {
    sessions: Vec<minicoding_core::model::SessionMeta>,
}

/// HTTP 错误响应（实现 `IntoResponse`，handler 用 `Result<T, HttpError>` 返回）。
#[derive(Debug)]
pub struct HttpError {
    pub status: StatusCode,
    pub message: String,
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

/// S2：本机来源判定（解析 URI host 精确匹配，防 `http://localhost.evil.com` 伪装）。
fn is_local_origin(origin: &axum::http::HeaderValue, _parts: &axum::http::request::Parts) -> bool {
    origin
        .to_str()
        .ok()
        .and_then(|s| axum::http::Uri::try_from(s).ok())
        .and_then(|uri| uri.host().map(str::to_string))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"))
}

/// 构造 axum Router。
///
/// `web_dir` 设为 `Some(dir)` 时在 API/SSE 路由之外挂载 `ServeDir`（M9 `--web`，
/// 见 `design.md` §26.7），未匹配静态文件的路径 fallback 到 `index.html`（SPA
/// history 路由）。
///
/// `cors_origins` 空列表 = 默认仅允许**本机来源**（`localhost`/`127.0.0.1`/`[::1]`，任意
/// 端口——开发默认，S2 防浏览器 drive-by）；非空 = 仅允许列出的精确来源
/// （生产部署，M9 `--cors-origin`，见 `design.md` §26.6）。`*` 通配不再支持。
fn build_router(
    state: AppState,
    web_dir: Option<&Utf8PathBuf>,
    cors_origins: &[String],
    auth_token: Option<&str>,
) -> Router {
    let cors = if cors_origins.is_empty() {
        CorsLayer::new()
            .allow_origin(AllowOrigin::predicate(is_local_origin))
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
        .route(
            "/sessions/{id}/permissions/pending",
            get(pending_permissions),
        )
        .route("/sessions/{id}/permissions/{pid}", post(resolve_permission))
        // W-11 项目工作区（见 `docs/design.md` §26.9）
        .route(
            "/sessions/{id}/workspace",
            get(crate::workspace::workspace_root).post(crate::workspace::workspace_switch),
        )
        .route(
            "/sessions/{id}/workspace/list",
            get(crate::workspace::workspace_list),
        )
        .route(
            "/sessions/{id}/workspace/read",
            get(crate::workspace::workspace_read),
        )
        .route(
            "/sessions/{id}/workspace/diff",
            get(crate::workspace::workspace_diff),
        )
        .route("/config", get(server_config))
        .route("/metrics", get(prometheus_metrics))
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

    // S1：鉴权中间件（除 /health 外强制 Bearer token 或 ?token=，OPTIONS 预检放行由
    // CORS 层应答——auth 在内层、CORS 在外层，预检请求不触达 auth）
    let app = if let Some(token) = auth_token {
        let expected = token.to_string();
        app.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let expected = expected.clone();
                async move {
                    let path = req.uri().path();
                    let authorized = path == "/health"
                        || req.method() == axum::http::Method::OPTIONS
                        || request_authorized(&req, &expected);
                    if !authorized {
                        return axum::http::StatusCode::UNAUTHORIZED.into_response();
                    }
                    next.run(req).await
                }
            },
        ))
    } else {
        app
    };
    app.layer(cors).with_state(state)
}

/// S1：常量时间字符串比较（防时序侧信道逐字节猜 token）。
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// S1：请求是否携带有效凭证（`Authorization: Bearer <t>` 或 `?token=<t>`——后者仅供
/// 浏览器 `EventSource` 使用，其无法自定义请求头）。
fn request_authorized(req: &axum::extract::Request, expected: &str) -> bool {
    if let Some(v) = req.headers().get(axum::http::header::AUTHORIZATION)
        && let Ok(s) = v.to_str()
        && let Some(bearer) = s.strip_prefix("Bearer ")
    {
        return constant_time_eq(bearer, expected);
    }
    req.uri().query().is_some_and(|q| {
        q.split('&').any(|kv| {
            kv.split_once('=')
                .is_some_and(|(k, v)| k == "token" && constant_time_eq(v, expected))
        })
    })
}

/// 启动 HTTP server（阻塞当前 task）。
///
/// # Errors
/// Server bind 失败或运行时错误时返回 `anyhow::Error`。
pub async fn serve(cfg: ServerConfig) -> anyhow::Result<()> {
    let params = default_params(&cfg);
    let permission_timeout = Duration::from_secs(cfg.permission_timeout_sec);
    let mgr = Arc::new(SessionManager::new(params, permission_timeout));
    let state = AppState {
        mgr,
        cfg: cfg.clone(),
    };

    if cfg.auth_token.is_none() {
        tracing::warn!(
            "API 鉴权已禁用（auth_token=None）：本机任意进程可读取会话、代答权限、执行命令"
        );
    }
    let app = build_router(
        state,
        cfg.web_dir.as_ref(),
        &cfg.cors_origins,
        cfg.auth_token.as_deref(),
    );
    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .map_err(|e| anyhow::anyhow!("bind {addr} 失败: {e}", addr = cfg.bind))?;
    // 输出实际监听地址（端口 0 时由 OS 分配，sidecar 依赖此日志解析端口）
    let local_addr = listener
        .local_addr()
        .map_err(|e| anyhow::anyhow!("获取监听地址失败: {e}"))?;
    tracing::info!(addr = %local_addr, web_dir = ?cfg.web_dir, "minicoding-server 启动");
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

/// `GET /config` — 返回 server 当前默认配置（不含 API key，C-04）。
///
/// 供前端设置面板加载真实默认值（Web 模式编辑设置时兜底，Tauri 模式
/// 与 `config.toml` 交叉核对）。新会话可通过 `CreateSessionBody` 会话级覆盖。
async fn server_config(State(state): State<AppState>) -> Json<ServerConfigResponse> {
    let c = &state.cfg;
    let config_revision = minicoding_core::config::load_config().map_or(0, |cfg| cfg.revision);
    Json(ServerConfigResponse {
        provider_kind: c.provider_kind.clone(),
        provider_name: c.provider_name.clone(),
        api_base: c.api_base.clone(),
        model: c.model.clone(),
        timeout_sec: c.timeout_sec,
        max_retries: c.max_retries,
        small_model: c.small_model.clone(),
        turn_timeout_sec: c.turn_timeout_sec,
        compress: c.compress,
        permission_timeout_sec: c.permission_timeout_sec,
        preset: c.preset.clone(),
        config_revision,
    })
}

/// `POST /sessions` — 创建新会话。
async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionBody>,
) -> Result<Json<CreateSessionResponse>, HttpError> {
    let default = state.mgr.default_params();
    let mut params = ServerRuntimeParams {
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
        sandbox_policy: default.sandbox_policy.clone(),
        timeout_sec: body.timeout_sec.unwrap_or(default.timeout_sec),
        max_retries: body.max_retries.unwrap_or(default.max_retries),
        small_model: body.small_model.or(default.small_model.clone()),
        turn_timeout_sec: body.turn_timeout_sec.unwrap_or(default.turn_timeout_sec),
        compress: body.compress.unwrap_or(default.compress),
    };
    // preset 解析（会话级覆盖）：`full-access` 强制 `BypassPermissions` 全自动 +
    // `DangerFullAccess` 沙箱外运行（C-22：显式选定 + red 警告，见 build_preset_policy）
    if let Some(preset_str) = body.preset.as_deref() {
        // S3/C-22：高危预设需请求体显式二次确认（UI 先弹红色警告框），仅日志不够
        ensure_danger_preset_confirmed(preset_str, body.confirm_danger)?;
        let (mode, policy, warning) = build_preset_policy(preset_str, &params.workdir)?;
        // body.permission_mode 显式指定时优先于 preset 的默认模式
        if params.permission_mode == PermissionMode::Default {
            params.permission_mode = mode;
        }
        params.sandbox_policy = policy;
        if let Some(w) = warning {
            tracing::warn!("{w}");
        }
    }
    // Plan 模式（C-25：先规划后执行）：`plan_mode` 请求且未显式指定模式时生效
    if body.plan_mode && params.permission_mode == PermissionMode::Default {
        params.permission_mode = PermissionMode::Plan;
    }

    let session = state.mgr.create_session(Some(params))?;
    let session_id = session.session_id().clone();
    Ok(Json(CreateSessionResponse { session_id }))
}

/// 高危预设清单（C-22：沙箱降级或权限全自动）。
const DANGER_PRESETS: &[&str] = &["full-access", "external-sandbox"];

/// S3/C-22：高危预设必须携带 `confirm_danger: true`（UI 红色警告确认后回传）。
///
/// # Errors
/// 高危预设未确认时返回 400 与引导信息。
fn ensure_danger_preset_confirmed(
    preset: &str,
    confirm_danger: Option<bool>,
) -> Result<(), HttpError> {
    if DANGER_PRESETS.contains(&preset) && confirm_danger != Some(true) {
        return Err(HttpError {
            status: axum::http::StatusCode::BAD_REQUEST,
            message: format!(
                "预设 `{preset}` 属高危配置（C-22）：沙箱降级/权限全自动。\
                 请在 UI 确认红色警告后在请求体携带 \"confirm_danger\": true 重试"
            ),
        });
    }
    Ok(())
}

/// 解析安全预设为 `(PermissionMode, SandboxPolicy, 警告信息)`。
///
/// - `auto`：默认（`WorkspaceWrite` 工作区内可写，其余 Ask）；
/// - `read-only`：`ReadOnly`（文件只读，命令/网络仍 Ask）；
/// - `external-sandbox`：`ExternalSandbox`（依赖容器/外部沙箱隔离，C-22）；
/// - `full-access`：`DangerFullAccess` + `BypassPermissions` 全自动——沙箱外运行，
///   仅受信隔离容器内使用（C-22：red 警告 + 显式选定；API 传参视为显式选定，
///   返回警告供调用方/日志展示）。
fn build_preset_policy(
    preset: &str,
    workdir: &Utf8PathBuf,
) -> Result<(PermissionMode, SandboxPolicy, Option<String>), HttpError> {
    match preset {
        "auto" => Ok((
            PermissionMode::Default,
            SandboxPolicy::WorkspaceWrite {
                workdir: workdir.clone(),
                writable: Vec::new(),
            },
            None,
        )),
        "read-only" => Ok((PermissionMode::Default, SandboxPolicy::ReadOnly, None)),
        "external-sandbox" => Ok((
            PermissionMode::Default,
            SandboxPolicy::ExternalSandbox,
            None,
        )),
        "full-access" => Ok((
            PermissionMode::BypassPermissions,
            SandboxPolicy::DangerFullAccess,
            Some(
                "WARNING: 会话以 full-access 预设运行（沙箱外 + 权限全自动）——                 仅限受信隔离容器内使用（C-22）"
                    .to_string(),
            ),
        )),
        other => Err(HttpError {
            status: axum::http::StatusCode::BAD_REQUEST,
            message: format!(
                "未知预设 `{other}`，可选：auto / read-only / external-sandbox / full-access"
            ),
        }),
    }
}

/// `GET /metrics` — Prometheus text format 快照（P9）。
///
/// 纳入鉴权（与业务端点一致）；监控侧配 token 拉取。
async fn prometheus_metrics() -> impl IntoResponse {
    let body = minicoding_core::metrics::snapshot_prometheus();
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

/// `GET /sessions` — 列出所有会话（内存活跃 + 磁盘历史合并）。
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
    let session = state.mgr.get_or_load(&session_id).await?;
    let tasks = session
        .task_state
        .lock()
        .expect("task_state mutex poisoned")
        .clone();
    Ok(Json(GetSessionResponse {
        session_id,
        messages,
        tasks,
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
    state.mgr.cancel(&session_id).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// `GET /sessions/{id}/events` — SSE 事件流。
async fn sse_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>>, HttpError> {
    let session = state.mgr.get_or_load(&session_id).await?;

    let headers = headers.clone();
    let header = headers.get("last-event-id").and_then(|v| v.to_str().ok());
    let stream = if header.is_some() {
        // 断线重连（浏览器 EventSource 自动携带 Last-Event-ID）：从断点恢复重放
        let last_seq = sse::parse_last_event_id(header);
        sse::sse_stream(session, last_seq)
    } else {
        // 首次连接：只推新事件，不回放历史（历史 permission_requested 若重放
        // 会让前端弹窗 pid 错乱，见 `sse.rs::sse_live` 说明）
        sse::sse_live(session)
    };
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

/// `GET /sessions/{id}/permissions/pending` — 未决权限请求快照。
///
/// SSE 断线/页面刷新后，前端拉取此快照恢复权限弹窗（`PermissionRequested`
/// 是瞬态事件，重连重放不可用，见 `sse.rs` 模块注释）。
async fn pending_permissions(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, HttpError> {
    let session = state.mgr.get_or_load(&session_id).await?;
    let pending = crate::prompter::list_pending_permissions(&session.pending_permissions).await;
    Ok(Json(serde_json::json!({"pending": pending})))
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

/// 生成 API 鉴权 token（S1）。委托 `core::util`（与 CLI/desktop 共用同一策略）。
#[must_use]
pub fn generate_auth_token() -> String {
    minicoding_core::util::generate_auth_token()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    // ── S3：danger preset 二次确认 ──

    #[test]
    fn danger_preset_without_confirm_rejected() {
        for preset in ["full-access", "external-sandbox"] {
            let err = ensure_danger_preset_confirmed(preset, None).expect_err("应拒绝");
            assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
            assert!(err.message.contains("confirm_danger"), "{}", err.message);
            let err =
                ensure_danger_preset_confirmed(preset, Some(false)).expect_err("false 也应拒绝");
            assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn danger_preset_with_confirm_accepted() {
        for preset in DANGER_PRESETS {
            ensure_danger_preset_confirmed(preset, Some(true)).expect("确认后应放行");
        }
    }

    #[test]
    fn safe_presets_need_no_confirm() {
        for preset in ["auto", "read-only"] {
            ensure_danger_preset_confirmed(preset, None).expect("安全预设无需确认");
        }
    }

    // ── S2：本机来源判定 ──

    fn origin_header(s: &str) -> axum::http::HeaderValue {
        axum::http::HeaderValue::from_str(s).expect("header")
    }
    fn parts() -> axum::http::request::Parts {
        axum::http::Request::builder()
            .body(())
            .expect("req")
            .into_parts()
            .0
    }

    #[test]
    fn local_origins_allowed_any_port() {
        let p = parts();
        for o in [
            "http://localhost",
            "http://localhost:5173",
            "http://127.0.0.1:8080",
            "http://[::1]:3000",
        ] {
            assert!(is_local_origin(&origin_header(o), &p), "{o} 应允许");
        }
    }

    // ── S1：鉴权中间件 ──

    use axum::body::Body;
    use axum::http::header::AUTHORIZATION;
    use tower::ServiceExt;

    /// 构造带鉴权的测试 app（SessionManager 用假 provider 参数，不发起真实请求）。
    fn test_app(auth_token: Option<&str>) -> axum::Router {
        let cfg = ServerConfig {
            bind: "127.0.0.1:0".parse().expect("bind"),
            provider_kind: "openai".into(),
            provider_name: None,
            api_base: "http://localhost:1".into(),
            api_key: String::new(),
            model: "test-model".into(),
            workdir: camino::Utf8PathBuf::from("."),
            system: None,
            permission_timeout_sec: 1,
            web_dir: None,
            cors_origins: Vec::new(),
            auth_token: auth_token.map(str::to_string),
            preset: "auto".into(),
            timeout_sec: 1,
            max_retries: 1,
            small_model: None,
            turn_timeout_sec: 1,
            compress: false,
        };
        let params = crate::ServerRuntimeParams {
            provider_kind: cfg.provider_kind.clone(),
            provider_name: None,
            api_base: cfg.api_base.clone(),
            api_key: String::new(),
            model: cfg.model.clone(),
            workdir: cfg.workdir.clone(),
            system: None,
            permission_mode: minicoding_core::policy::PermissionMode::Default,
            sandbox_policy: minicoding_core::sandbox::SandboxPolicy::ReadOnly,
            timeout_sec: 1,
            max_retries: 1,
            small_model: None,
            turn_timeout_sec: 1,
            compress: false,
        };
        let mgr = Arc::new(SessionManager::new(params, Duration::from_secs(1)));
        build_router(AppState { mgr, cfg }, None, &[], auth_token)
    }

    #[tokio::test]
    async fn auth_required_without_token() {
        let app = test_app(Some("secret-token"));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "无 token 应 401");
    }

    #[tokio::test]
    async fn auth_bearer_token_accepted() {
        let app = test_app(Some("secret-token"));
        let req = axum::http::Request::builder()
            .uri("/sessions")
            .header(AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .expect("req");
        let resp = app.oneshot(req).await.expect("resp");
        assert_eq!(resp.status(), StatusCode::OK, "正确 Bearer token 应放行");
    }

    #[tokio::test]
    async fn auth_query_token_accepted_for_sse() {
        let app = test_app(Some("secret-token"));
        // EventSource 场景：?token= 查询参数（此处用 /config 端点验证 query 通道）
        let req = axum::http::Request::builder()
            .uri("/config?token=secret-token")
            .body(Body::empty())
            .expect("req");
        let resp = app.oneshot(req).await.expect("resp");
        assert_eq!(resp.status(), StatusCode::OK, "?token= 应放行");
    }

    #[tokio::test]
    async fn health_exempt_from_auth() {
        let app = test_app(Some("secret-token"));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK, "/health 免鉴权（liveness）");
    }

    #[tokio::test]
    async fn no_auth_config_allows_all() {
        let app = test_app(None);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/sessions")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_ne!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "关闭鉴权时不应 401"
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_requires_auth_and_renders_prometheus() {
        // 无 token → 401
        let app = test_app(Some("secret-token"));
        let resp = app
            .oneshot(axum::http::Request::builder().uri("/metrics").body(Body::empty()).expect("req"))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 带 token → 200 + Prometheus text format（先注入一个计数）
        minicoding_core::metrics::set_active_sessions(1);
        let app = test_app(Some("secret-token"));
        let req = axum::http::Request::builder()
            .uri("/metrics")
            .header(AUTHORIZATION, "Bearer secret-token")
            .body(Body::empty())
            .expect("req");
        let resp = app.oneshot(req).await.expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.contains("text/plain"), "content-type: {ct}");
        let body = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body")
                .to_vec(),
        )
        .expect("utf8");
        assert!(
            body.starts_with("# TYPE minicoding_active_sessions") || body.contains("# TYPE "),
            "应含 TYPE 注释行: {body}"
        );
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abx"));
        assert!(!constant_time_eq("abc", "ab"));
    }

    #[test]
    fn non_local_origins_rejected() {
        let p = parts();
        for o in [
            "https://evil.com",
            "http://localhost.evil.com",
            "http://evil-localhost.com",
            "null",
        ] {
            assert!(!is_local_origin(&origin_header(o), &p), "{o} 应拒绝");
        }
    }
}
