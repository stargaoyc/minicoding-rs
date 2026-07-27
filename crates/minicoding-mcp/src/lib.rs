//! # minicoding-mcp
//!
//! `MCP` client/server：实现 `core::mcp::McpClient` trait。
//!
//! 基于官方 `rmcp` 2.x `SDK`（modelcontextprotocol/rust-sdk），对齐 `MCP` 2025-11-25
//! spec，支持 stdio + streamable `HTTP` + OAuth + `#[tool]` 宏 + schemars `JSON` Schema
//! 生成。**不自研** stdio/http 薄封装（见 `tech-stack.md` §11.1、§13）。
//!
//! ## 设计要点
//!
//! - **工具命名**：`mcp__<server>__<tool>`（见 `design.md` §19.3），与权限规则通配匹配兼容；
//! - **project 作用域批准**：首次遇到含 `.minicoding/mcp.json` 的仓库时逐个 server 弹窗，
//!   防恶意仓库植入（C-24）；
//! - **凭证隔离**：`MCP` server 子进程不继承 minicoding 凭证环境变量（C-04）；
//! - **`required` 语义**：`required = true` 的 server 启动失败则 minicoding 拒绝启动；
//!   `required = false`（默认）失败仅 warn 跳过。
//!
//! 当前 M0 阶段：仅占位骨架（T-M0-1），rmcp 依赖与实现见 M4（T-M4-5）。
//!
//! 详见 `docs/modules.md` §8、`docs/design.md` §19。

#![deny(clippy::all, clippy::pedantic)]
