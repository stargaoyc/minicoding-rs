//! # minicoding-context
//!
//! 上下文管理实现：实现 `core::context::ContextManager` trait。
//!
//! 职责：token 预算计算、消息权重模型、4 级压缩管道（裁剪→摘要→滚动→硬截断）、
//! 压缩熔断与防 Thrash、状态保留清单、压缩失败降级链。
//!
//! 依赖 `minicoding-core` 的 trait 与数据模型；摘要压缩需调 LLM，通过 `LlmProvider`
//! trait 注入（不直接依赖 providers crate）。
//!
//! 详见 `docs/modules.md` §2、`docs/design.md` §3。

#![deny(clippy::all, clippy::pedantic)]
