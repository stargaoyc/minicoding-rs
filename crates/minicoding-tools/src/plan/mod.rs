//! Plan 模式工具（`plan.exit`/`plan.list`，见 `design.md` §16.4）。
//!
//! `plan.exit` 由模型在 Plan 阶段完成 plan.md 后调用，携带"预批准命令"清单。
//! Runtime 接到调用后：
//! 1. 校验当前 `PermissionMode == Plan`（否则 `InvalidStateTransition`，C-25）；
//! 2. 切换 `PermissionMode` 为 `target_mode`（Default 或 `AcceptEdits`）；
//! 3. 缓存 `allowed_prompts` 到会话级状态，执行期命中即 `Allow`。
//!
//! `plan.list`（M-11 新增）列出当前 Plan 模式状态（`mode` + `allowed_prompts`），
//! 供模型在 Plan 阶段查询。
//!
//! 两工具均属 `Plan` 工具组，`SideEffect::None`（仅切换状态 + 缓存），可穿透
//! Plan 模式硬门（`is_read_only() == true`，C-25）。

mod exit;
mod list;

pub use exit::PlanExit;
pub use list::PlanList;

use minicoding_core::policy::PlanModeController;
use minicoding_core::tool::ToolRegistry;
use std::sync::Arc;

/// 注册 Plan 工具组到 `registry`。
///
/// `controller` 由 `Runtime::plan_controller()` 提供，共享 Runtime 的 `plan_state`。
pub fn register_plan_tools(registry: &mut ToolRegistry, controller: Arc<dyn PlanModeController>) {
    registry.register(Arc::new(PlanExit::new(controller.clone())));
    registry.register(Arc::new(PlanList::new(controller)));
}

#[cfg(test)]
mod tests {
    //! `register_plan_tools` 注册测试（覆盖率补全）。

    use super::*;
    use minicoding_core::model::PolicyError;
    use minicoding_core::policy::{PermissionMode, PlanModeSnapshot, PreApprovedPrompt};
    use minicoding_core::provider::BoxFuture;
    use tokio::sync::RwLock;

    /// 测试用 PlanModeController，直接持有 `RwLock<PlanModeSnapshot>`。
    struct StubController {
        state: Arc<RwLock<PlanModeSnapshot>>,
    }

    impl PlanModeController for StubController {
        fn snapshot(&self) -> BoxFuture<'_, PlanModeSnapshot> {
            let state = self.state.clone();
            Box::pin(async move { state.read().await.clone() })
        }

        fn exit_plan(
            &self,
            _allowed_prompts: Vec<PreApprovedPrompt>,
            _target_mode: PermissionMode,
        ) -> BoxFuture<'_, Result<(), PolicyError>> {
            Box::pin(async move { Ok(()) })
        }

        fn set_mode(&self, _mode: PermissionMode) -> BoxFuture<'_, ()> {
            Box::pin(async move {})
        }
    }

    fn make_controller() -> Arc<StubController> {
        Arc::new(StubController {
            state: Arc::new(RwLock::new(PlanModeSnapshot::default())),
        })
    }

    #[test]
    fn register_plan_tools_registers_both_tools() {
        let mut registry = ToolRegistry::new();
        register_plan_tools(&mut registry, make_controller());
        assert!(registry.get("plan.exit").is_some());
        assert!(registry.get("plan.list").is_some());
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn register_plan_tools_shared_controller() {
        let mut registry = ToolRegistry::new();
        register_plan_tools(&mut registry, make_controller());
        // 两个工具共享同一 controller（plan_state），快照一致
        assert_eq!(registry.get("plan.exit").unwrap().name(), "plan.exit");
        assert_eq!(registry.get("plan.list").unwrap().name(), "plan.list");
        // 均只读（C-25 穿透 Plan 硬门）
        assert!(registry.get("plan.exit").unwrap().is_read_only());
        assert!(registry.get("plan.list").unwrap().is_read_only());
    }

    #[test]
    fn register_plan_tools_tool_is_read_only_with_no_side_effect() {
        let mut registry = ToolRegistry::new();
        register_plan_tools(&mut registry, make_controller());
        let tool = registry
            .get("plan.exit")
            .expect("plan.exit should be registered");
        assert_eq!(tool.name(), "plan.exit");
        assert_eq!(tool.side_effect(), minicoding_core::model::SideEffect::None);
        assert!(tool.is_read_only());
    }

    // ---- StubController 方法调用覆盖 ----

    #[tokio::test]
    async fn stub_controller_snapshot_returns_default() {
        let ctrl = make_controller();
        let snap = ctrl.snapshot().await;
        assert_eq!(snap.mode, PermissionMode::Default);
        assert!(
            snap.allowed_prompts.is_empty(),
            "expected empty: snap.allowed_prompts"
        );
    }

    #[tokio::test]
    async fn stub_controller_exit_plan_succeeds() {
        let ctrl = make_controller();
        let result = ctrl.exit_plan(vec![], PermissionMode::Default).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn stub_controller_set_mode_is_noop() {
        let ctrl = make_controller();
        // set_mode 是 no-op，不应 panic
        ctrl.set_mode(PermissionMode::AcceptEdits).await;
    }
}
