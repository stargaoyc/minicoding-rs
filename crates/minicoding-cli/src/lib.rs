//! # minicoding-cli（lib）
//!
//! CLI frontend 业务逻辑库。`main.rs`（bin）仅负责参数解析与退出码，所有模块逻辑
//! 在此暴露，便于 `minicoding-tui` 等同层 frontend 复用 `build_runtime` 组装链路
//! （见 `docs/modules.md` §12、§13.3）。
//!
//! 详见 `docs/modules.md` §12。

pub mod builder;
pub mod commands;
pub mod cred;
pub mod otel_init;
pub mod session;
