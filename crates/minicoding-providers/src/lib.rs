//! # minicoding-providers
//!
//! LLM Provider 实现：实现 `core::provider::LlmProvider`/`Tokenizer` trait。
//!
//! 支持 `OpenAI` 兼容、`Anthropic`、`Ollama` 三类 provider，每个 provider 内部统一返回
//! `BoxStream<Result<Delta>>`，转换逻辑隔离。
//!
//! ## 设计要点
//!
//! - **密钥安全**：密钥从环境变量或 `OS` keyring 读取，绝不接受配置文件明文（C-04），
//!   日志中密钥脱敏（前 4 字符 + `***`）；
//! - **`TLS` rustls**：使用 `reqwest` + `rustls-tls`，避免系统 OpenSSL 依赖（P-09）；
//! - **重试与超时**：在 `common::retry` 统一实现（指数退避、429 Retry-After），装饰器
//!   包裹 stream；
//! - **`SSE` 解析**：`common::sse` 处理流式响应边界（分片、空 data、`[DONE]`）。
//!
//! 详见 `docs/modules.md` §10、`docs/design.md` §4。

#![deny(clippy::all, clippy::pedantic)]

mod openai;
mod tokenizer;

pub use openai::{OpenAiProvider, PROVIDER_ID};
pub use tokenizer::{TiktokenKind, TiktokenTokenizer};
