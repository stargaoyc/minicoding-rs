//! # minicoding-protocol
//!
//! 前后端协议契约：JSON-RPC 2.0 wire types + `Event`/`Command` DTO。
//!
//! 独立于实现 crate，定义 CLI / TUI / HTTP Server / ACP 适配器共用的线协议类型。
//! `Event` 携带 `seq: u64` 单调递增序列号，支持 SSE cursor 恢复；broadcast 溢出时
//! 发 `RehydrateRequired` 通知客户端重拉 snapshot。
//!
//! ## 设计要点
//!
//! - **协议与实现解耦**：wire types 集中在此 crate，`minicoding-server`（HTTP/SSE）、
//!   `minicoding-cli`（stdio）、`minicoding-tui`（channel）共用同一套 DTO；
//! - **cursor 恢复**：SSE 流携带 `cursor`（event seq），客户端断连后从 cursor 恢复，
//!   避免丢失事件；
//! - **Rehydrate 信号**：`broadcast` 溢出时发 `RehydrateRequired` delta，客户端重拉
//!   snapshot 而非静默丢事件。
//!
//! 当前 M0 阶段：仅占位骨架（T-M0-1），实现见 M5（protocol）+ M6/M8（server）。
//!
//! 详见 `docs/modules.md` §15、`docs/design.md` §23。

#![deny(clippy::all, clippy::pedantic)]
