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
