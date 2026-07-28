//! # minicoding-server
//!
//! HTTP/SSE server + ACP（Agent Client Protocol）适配器。
//!
//! 提供 JSON-RPC 2.0 over HTTP/SSE 接口，支持多客户端并发会话；ACP stdio 适配器
//! 可被支持 ACP 的客户端（如 Zed）嵌入。复用 `minicoding-protocol` 的 wire types。
//!
//! ## 设计要点
//!
//! - **SSE cursor 恢复**：事件流携带 `cursor`（event seq），客户端断连后从 cursor
//!   恢复，不丢事件；
//! - **Rehydrate 信号**：`broadcast` 溢出时发 `RehydrateRequired`，客户端重拉 snapshot；
//! - **多会话并发**：HTTP path 带 `session_id`，支持多 session 并发；
//! - **ACP stdio**：作为 `minicoding serve --acp` 子模式，stdio 传输 JSON-RPC。
//!
//! 当前 M0 阶段：仅占位骨架（T-M0-1），实现见 M6/M8。
//!
//! 详见 `docs/modules.md` §16、`docs/design.md` §24。

#![deny(clippy::all, clippy::pedantic)]

fn main() {
    // M0 占位：HTTP/SSE server 实现见 M6/M8
    println!("minicoding-server - HTTP/SSE server (skeleton, M6/M8)");
}
