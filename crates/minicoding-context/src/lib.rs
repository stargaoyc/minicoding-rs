//! # minicoding-context
//!
//! 上下文管理实现：实现 `core::context::ContextManager` trait。
//!
//! M1 阶段提供 `SimpleContextManager`：仅持有消息列表与系统提示词，无压缩。
//! M3 提供 `ContextManagerImpl`（注入 `Tokenizer` 精确计数、`TokenBudget` 预算控制、
//! `compress` 压缩入口占位）、`TokenBudget`（预算计算）、`message_weight`（消息权重模型）。
//! 后续将实现完整压缩管道（裁剪→摘要→滚动→硬截断）、压缩熔断与防 Thrash、状态保留清单、
//! 压缩失败降级链。
//!
//! 依赖 `minicoding-core` 的 trait 与数据模型；摘要压缩需调 LLM，通过 `LlmProvider`
//! trait 注入（不直接依赖 providers crate）。
//!
//! 详见 `docs/modules.md` §2、`docs/design.md` §3。

#![deny(clippy::all, clippy::pedantic)]

mod budget;
mod compress;
mod manager;
mod simple;
mod weight;

pub use budget::TokenBudget;
pub use compress::{
    CircuitBreaker, CircuitBreakerConfig, CompressResult, StateKeep, compress_pipeline,
    summarize_with_fallback,
};
pub use manager::ContextManagerImpl;
pub use simple::SimpleContextManager;
pub use weight::message_weight;
