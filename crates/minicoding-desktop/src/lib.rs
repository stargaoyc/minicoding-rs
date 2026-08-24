//! # minicoding-desktop
//!
//! Tauri 2.x 桌面壳（M9，低优先级）。启动 `minicoding-server` 作为 sidecar 进程，
//! Tauri `WebView` 加载 `minicoding-web` 构建产物。架构设计见 `docs/design.md` §26、
//! 模块职责见 `docs/modules.md` §19。
//!
//! ## 设计要点
//!
//! - **sidecar 管理**：Tauri 启动 `minicoding-server --http --bind 127.0.0.1:0`（随机端口），
//!   读取 stdout 获取实际监听端口，注入前端；
//! - **IPC 桥接**：前端通过 Tauri `invoke('start_session')` 获取 sidecar 端口，后续通信
//!   走 HTTP/SSE（同源，无 CORS 问题）；
//! - **凭证**：复用 OS keyring（与 CLI `cred.rs` 共享 `KEYRING_SERVICE = "minicoding"`，C-04）；
//! - **安全**：Tauri 默认禁用远程内容，仅加载本地 `dist/`；CSP `script-src
//!   'self'`（2026-08-23 审查遗留#4 收紧，移除 unsafe-inline）；style-src 保留
//!   'unsafe-inline'（前端内联样式所需）。
//!
//! ## Feature gate
//!
//! `desktop` feature 启用 Tauri 重依赖（需系统 webview 运行时：`webkit2gtk` Linux /
//! `WebKit` macOS / `WebView2` Windows）。不启用时本 crate 为 stub（仅提供类型定义与
//! sidecar 解析逻辑，不拉入 Tauri），保证 workspace 在无 webview 环境编译通过。
//!
//! 详见 `docs/design.md` §26.5（Tauri sidecar 集成）。

#![deny(clippy::all, clippy::pedantic)]

use camino::Utf8PathBuf;

pub mod config;
pub mod sidecar;

#[cfg(feature = "desktop")]
pub mod tray;

/// sidecar 会话信息（返回给前端）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    /// sidecar 监听端口。
    pub port: u16,
    /// sidecar 进程 PID。
    pub pid: u32,
    /// API 鉴权 token（S1）：desktop 生成并传给 sidecar，前端请求时携带。
    pub token: String,
}

/// 解析 `minicoding-server` sidecar 的 web 目录。
///
/// 桌面模式默认托管打包内嵌的 `minicoding-web/dist`。开发模式可通过
/// `MINICODING_WEB_DIR` 环境变量指向源码目录。
#[must_use]
pub fn resolve_web_dir() -> Option<Utf8PathBuf> {
    // SAFETY: 单次读取环境变量，无并发风险（Rust 2024 edition 标记 unsafe）
    if let Ok(dir) = std::env::var("MINICODING_WEB_DIR") {
        return Some(Utf8PathBuf::from(dir));
    }
    None
}

/// 启动 sidecar 并返回端口（无 Tauri 依赖版本）。
///
/// `desktop` feature 启用时，`main.rs` 通过 Tauri `invoke` 调用此函数；
/// 未启用时供测试与 sidecar 逻辑复用。
///
/// # Errors
/// sidecar 启动失败或端口解析失败时返回错误。
pub async fn start_sidecar() -> anyhow::Result<SessionInfo> {
    sidecar::spawn_sidecar_standalone().await
}
