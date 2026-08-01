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
#[derive(thiserror::Error, Debug)]
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
