//! Plan 模式工具（`plan.exit`，见 `design.md` §16.4）。
//!
//! `plan.exit` 由模型在 Plan 阶段完成 plan.md 后调用，携带"预批准命令"清单。
//! Runtime 接到调用后：
//! 1. 校验当前 `PermissionMode == Plan`（否则 `InvalidStateTransition`，C-25）；
//! 2. 切换 `PermissionMode` 为 `target_mode`（Default 或 `AcceptEdits`）；
//! 3. 缓存 `allowed_prompts` 到会话级状态，执行期命中即 `Allow`。
//!
//! 该工具属 `Plan` 工具组，`SideEffect::None`（仅切换状态 + 缓存），可穿透
//! Plan 模式硬门（`is_read_only() == true`，C-25）。

mod exit;

pub use exit::PlanExit;

use minicoding_core::policy::PlanModeController;
use minicoding_core::tool::ToolRegistry;
use std::sync::Arc;

/// 注册 Plan 工具组到 `registry`。
///
/// `controller` 由 `Runtime::plan_controller()` 提供，共享 Runtime 的 `plan_state`。
pub fn register_plan_tools(registry: &mut ToolRegistry, controller: Arc<dyn PlanModeController>) {
    registry.register(Arc::new(PlanExit::new(controller)));
}
