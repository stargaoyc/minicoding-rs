//! # minicoding-sdk
//!
//! 嵌入 `SDK`（M8）：为第三方 Rust 程序提供高层嵌入 `API`，隐藏 `Runtime` 细节。
//!
//! ## 公共 `API`
//!
//! ```rust,ignore
//! pub struct Client { runtime: Runtime }
//!
//! impl Client {
//!     pub fn builder() -> ClientBuilder;
//!     pub async fn ask(&self, prompt: &str) -> Result<String>;
//!     pub async fn ask_stream(&self, prompt: &str) -> impl Stream<Item = Result<Delta>>;
//!     pub async fn run_task(&self, task: &str) -> Result<TaskReport>;
//!     pub fn on_event(&self, f: impl Fn(Event)) -> Subscription;
//! }
//! ```
//!
//! ## 设计要点
//!
//! - 默认无副作用权限策略，调用方需显式启用；
//! - 提供 `CallbackPrompter`（来自 `minicoding-policy`）供 `SDK` 用户闭包处理权限交互；
//! - 所有 `API` `Send + Sync`，可在多 tokio 任务中共享。
//!
//! 当前 M0 阶段：仅占位骨架（T-M0-1），实现见 M8。
//!
//! 详见 `docs/modules.md` §14、`docs/roadmap.md` M8。

#![deny(clippy::all, clippy::pedantic)]
