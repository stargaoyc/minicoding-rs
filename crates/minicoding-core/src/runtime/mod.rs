//! Runtime 聚合根 + Agent 循环 + 事件总线。
//!
//! `Runtime` 持有所有可替换能力（`Arc<dyn Trait>`），驱动单轮对话循环。
//! M1 简化：不接入权限/沙箱/Hook（M2+ 完整实现）。
//!
//! 详见 `design.md` §1-§2。

mod accumulator;
mod builder;
mod event;
mod plan_handle;
pub mod repeat_guard;
mod rt;

pub use accumulator::DeltaAccumulator;
pub use builder::RuntimeBuilder;
pub use event::{Event, EventBus};
pub use rt::Runtime;
