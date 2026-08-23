//! Runtime 聚合根 + Agent 循环 + 事件总线。
//!
//! `Runtime` 持有所有可替换能力（`Arc<dyn Trait>`），驱动单轮对话循环。
//! `rt.rs` 保留循环主链（`run_turn`/stream/工具分桶），内聚单元按职责拆分：
//! `sourcing`（事件溯源）/`permission`（权限+Hook 管道）/`denial`（沙箱拒绝与
//! 回退）/`hot_config`（白名单热更新）/`workdir`（工作区切换）。
//!
//! 详见 `design.md` §1-§2。

mod accumulator;
mod builder;
mod denial;
mod event;
pub(crate) mod hot_config;
mod permission;
mod plan_handle;
pub mod repair;
pub mod repeat_guard;
mod rt;
mod sourcing;
mod workdir;

pub use accumulator::DeltaAccumulator;
pub use builder::RuntimeBuilder;
pub use event::{Event, EventBus};
pub use rt::Runtime;
