//! 错误类型层次（库 crate 用 `thiserror`，见 AGENTS.md §2.3）。
//!
//! `RuntimeError` 是顶层错误，各领域错误通过 `#[from]` 转换。边界 crate（cli/sdk）
//! 再转为 `anyhow::Error` 输出。

use std::time::Duration;

/// 顶层运行时错误。
#[derive(thiserror::Error, Debug)]
pub enum RuntimeError {
    #[error("llm: {0}")]
    Llm(#[from] LlmError),
    #[error("tool `{tool}`: {source}")]
    Tool {
        tool: String,
        #[source]
        source: ToolError,
    },
    #[error("permission denied: {0}")]
    Permission(String),
    #[error("context budget exceeded ({used}/{budget})")]
    BudgetExceeded { used: usize, budget: usize },
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("memory: {0}")]
    Memory(#[from] MemoryError),
    #[error("journal: {0}")]
    Journal(#[from] JournalError),
    #[error("mcp: {0}")]
    Mcp(#[from] McpError),
    #[error("sandbox: {0}")]
    Sandbox(#[from] crate::sandbox::SandboxError),
    #[error("hook fatal: {0}")]
    Hook(String),
    #[error("config: {0}")]
    Config(String),
    #[error("extension: {0}")]
    Extension(#[from] ExtensionError),
    /// Prompt 管道构建失败（contributor `build` 错误，见 `prompt::pipeline`）。
    #[error("prompt: {0}")]
    Prompt(#[from] PromptError),
    #[error("interrupted")]
    Interrupted,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<ToolError> for RuntimeError {
    fn from(e: ToolError) -> Self {
        Self::Tool {
            tool: String::new(),
            source: e,
        }
    }
}

/// LLM 调用错误。
#[derive(thiserror::Error, Debug, Clone)]
pub enum LlmError {
    #[error("network: {0}")]
    Network(String),
    #[error("rate limited; retry after {retry_after_ms:?}ms")]
    RateLimited { retry_after_ms: Option<u64> },
    #[error("server {status}: {body}")]
    Server { status: u16, body: String },
    #[error("client {status}: {body}")]
    Client { status: u16, body: String },
    #[error("content filtered: {reason}")]
    Filtered { reason: String },
    #[error("stream parse: {0}")]
    Parse(String),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("provider not configured")]
    NotConfigured,
}

impl LlmError {
    /// 该错误是否可重试（T-M6-3 重试装饰器用）。
    ///
    /// 可重试：`RateLimited`（429）、`Server`（5xx，瞬时故障）、`Network`（连接抖动）、
    /// `Timeout`（请求建立超时）。不可重试：`Client`（4xx，请求本身有问题，重试无意义）、
    /// `Filtered`（内容审核，重试同样结果）、`Parse`（响应格式错误，重试大概率复现）、
    /// `NotConfigured`（配置缺失）。
    ///
    /// 设计依据：见 `design.md` §10 错误分类与恢复策略。
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Server { .. } | Self::Network(_) | Self::Timeout(_)
        )
    }

    /// 返回 `RateLimited` 携带的 `Retry-After`（毫秒），其他错误返回 `None`。
    ///
    /// 重试装饰器优先用服务端建议的等待时间，缺省时回退到指数退避（见 `retry.rs`）。
    #[must_use]
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after_ms } => *retry_after_ms,
            _ => None,
        }
    }
}

/// 工具执行错误。
#[derive(thiserror::Error, Debug)]
pub enum ToolError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("invalid state transition: {0}")]
    InvalidStateTransition(String),
    #[error("path escapes workdir: {0}")]
    PathEscaped(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("execution: {0}")]
    Exec(String),
}

/// 存储错误。
#[derive(thiserror::Error, Debug)]
pub enum StorageError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize: {0}")]
    Serialize(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("corrupted: {0}")]
    Corrupted(String),
    /// 会话被跨进程文件锁占用（见 `rules.md` C-22、`SessionLock::acquire`）。
    #[error("session locked: {0}")]
    Locked(String),
    /// 会话文件由更新版本写入，当前版本不支持（M-02，对齐 dsh 格式版本拒绝）。
    #[error("session format unsupported: {0}")]
    FormatUnsupported(String),
}

/// 权限决策错误。
#[derive(thiserror::Error, Debug)]
pub enum PolicyError {
    #[error("policy: {0}")]
    Policy(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Prompt 管道错误。
#[derive(thiserror::Error, Debug)]
pub enum PromptError {
    #[error("prompt: {0}")]
    Prompt(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// 记忆错误（长期/Auto/项目文档加载与写入，见 `design.md` §8）。
#[derive(thiserror::Error, Debug)]
pub enum MemoryError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize: {0}")]
    Serialize(String),
    /// 记忆索引文件与正文不一致（mtime/size/hash 校验失败）。
    #[error("index inconsistent: {0}")]
    Inconsistent(String),
    /// 路径非 UTF-8 或不可解析。
    #[error("path: {0}")]
    Path(String),
}

/// 文件改动 journal 错误（见 `design.md` §17、`rules.md` C-28）。
///
/// `Conflict` 用于 `/undo` 冲突检测：文件已被外部编辑，当前内容与 `after` 不一致，
/// 不强行覆盖（C-28）。调用方将冲突文件记入 `UndoReport::failed_files`。
#[derive(thiserror::Error, Debug)]
pub enum JournalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// 文件已被外部改动，与记录的 `after` 不一致（冲突，不强行覆盖，C-28）。
    #[error("conflict: {0}")]
    Conflict(String),
    /// 没有可撤销的 entry（`steps` 超过已记录的 entry 数）。
    #[error("no entries to undo")]
    NoEntries,
    /// 路径越界（恢复路径经 `sandbox_path` 校验失败，C-03）。
    #[error("path escapes workdir: {0}")]
    PathEscaped(String),
}

/// MCP client 错误（见 `design.md` §19、`api.md` §11）。
#[derive(thiserror::Error, Debug)]
pub enum McpError {
    /// server 启动/握手失败（`required=true` 时 Runtime 拒绝启动）。
    #[error("server `{server}` start failed: {reason}")]
    StartFailed { server: String, reason: String },
    /// 工具调用失败（超时、server 返回错误、schema 不匹配）。
    #[error("call `{server}__{tool}` failed: {reason}")]
    CallFailed {
        server: String,
        tool: String,
        reason: String,
    },
    /// server 未就绪（未启动或已关闭）。
    #[error("server `{0}` not ready")]
    NotReady(String),
    /// 工具未在 server schema 中声明（C-09）。
    #[error("tool `{0}` not found in server schema")]
    ToolNotFound(String),
    /// project 作用域 server 未获用户批准（C-24）。
    #[error("server `{0}` not approved (project scope)")]
    NotApproved(String),
    /// 配置错误（transport 字段缺失、env 展开失败等）。
    #[error("config: {0}")]
    Config(String),
    /// IO 错误（stdio 子进程管道）。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// 扩展系统错误（见 `design.md` §23、`api.md` §3.12）。
///
/// `ExtensionHost::load_extension`/`unload_extension`/`on_config_changed` 与
/// `Extension::init`/`shutdown` 以及 `Registrar::register_*` 均返回此错误。
#[derive(thiserror::Error, Debug)]
pub enum ExtensionError {
    /// 扩展 id 重复（同 id 已加载）。
    #[error("extension already loaded: {0}")]
    AlreadyLoaded(String),
    /// 扩展未找到（`unload_extension`/`on_config_changed` 时 id 不存在）。
    #[error("extension not found: {0}")]
    NotFound(String),
    /// `Extension::init` 失败（注册能力时校验不通过、配置非法等）。
    #[error("extension `{extension}` init failed: {reason}")]
    InitFailed { extension: String, reason: String },
    /// `Extension::shutdown` 失败（资源释放异常）。
    #[error("extension `{extension}` shutdown failed: {reason}")]
    ShutdownFailed { extension: String, reason: String },
    /// manifest 非法（缺字段、版本不兼容、capabilities 与 Registrar 注册项不匹配）。
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    /// 注册能力越界（扩展未在 manifest 中声明对应 capability 却调 `register_*`）。
    #[error("capability `{0}` not declared in manifest")]
    CapabilityNotDeclared(String),
    /// 扩展申请的权限被 `PermissionPolicy` 拒绝（静态校验阶段，见 §23 安全约束）。
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// 配置 JSON 不符合 manifest 声明的 `config_schema`。
    #[error("config schema mismatch: {0}")]
    ConfigSchema(String),
    /// IO 错误（disk IPC 子进程加载、manifest 文件读取）。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// 序列化错误（manifest/config JSON）。
    #[error("serialize: {0}")]
    Serialize(#[from] serde_json::Error),
}
