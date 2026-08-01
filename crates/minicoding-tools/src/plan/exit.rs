//! `plan.exit`：退出 Plan 模式并提交计划（见 `design.md` §16.4）。

use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::policy::{PermissionMode, PlanModeController, PreApprovedPrompt};
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 退出 Plan 模式的工具（`SideEffect::None`，可穿透 Plan 硬门）。
///
/// 调用后 Runtime 触发 `Event::PermissionModeChanged { from: Plan, to: target_mode }`，
/// 并把 `allowed_prompts` 注入会话级 `PermissionPolicy` 缓存。
pub struct PlanExit {
    schema: ToolSchema,
    controller: Arc<dyn PlanModeController>,
}

impl PlanExit {
    /// 创建 `plan.exit` 工具实例。
    ///
    /// `controller` 由 `Runtime::plan_controller()` 提供，共享 Runtime 的
    /// `plan_state`（`Arc<RwLock<PlanModeSnapshot>>`）。
    #[must_use]
    pub fn new(controller: Arc<dyn PlanModeController>) -> Self {
        let schema = ToolSchema {
            name: "plan.exit".to_string(),
            description: "退出 Plan 模式并提交计划。仅在 Plan 模式下可调用（C-25）。\
调用后切换到 Default/AcceptEdits 模式，并把 allowed_prompts 注入会话级权限缓存，\
执行期匹配到的工具调用直接 Allow 跳过 prompter。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan_path": {
                        "type": "string",
                        "description": "plan.md 路径（如 .minicoding/plan.md）。"
                    },
                    "allowed_prompts": {
                        "type": "array",
                        "description": "预批准的命令清单，执行期命中即 Allow。",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": {"type": "string", "description": "工具名（如 shell.run）"},
                                "prompt": {"type": "string", "description": "命令前缀（如 cargo build）"}
                            },
                            "required": ["tool", "prompt"]
                        }
                    },
                    "target_mode": {
                        "type": "string",
                        "enum": ["default", "accept_edits"],
                        "description": "退出后切换到的模式（默认 default）。"
                    },
                    "plan_was_edited": {
                        "type": "boolean",
                        "description": "用户是否手改过 plan.md（仅供审计记录）。"
                    }
                },
                "required": ["plan_path"]
            }),
        };
        Self { schema, controller }
    }
}

/// `plan.exit` 工具的输入（见 `design.md` §16.4 `ExitPlanModeInput`）。
#[derive(Debug, Deserialize)]
struct ExitPlanModeInput {
    /// plan.md 路径（保留字段，Runtime 不强制校验存在，由用户在决策门检查）。
    plan_path: String,
    /// 预批准的命令清单（执行期命中即 Allow）。
    #[serde(default)]
    allowed_prompts: Vec<PreApprovedPrompt>,
    /// 退出后切换到的模式（默认 `Default`）。
    #[serde(default)]
    target_mode: TargetMode,
    /// 用户是否手改过 plan.md（仅供审计记录）。
    #[serde(default)]
    plan_was_edited: bool,
}

/// `target_mode` 反序列化辅助（CLI 友好的 kebab-case）。
#[derive(Debug, Default, Copy, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TargetMode {
    #[default]
    Default,
    AcceptEdits,
}

impl TargetMode {
    fn to_permission_mode(self) -> PermissionMode {
        match self {
            Self::Default => PermissionMode::Default,
            Self::AcceptEdits => PermissionMode::AcceptEdits,
        }
    }
}

impl Tool for PlanExit {
    fn name(&self) -> &'static str {
        "plan.exit"
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    /// `is_read_only() = true`：穿透 Plan 模式硬门（C-25）。
    /// 默认实现基于 `side_effect() == None`，此处显式覆盖以语义化声明。
    fn is_read_only(&self) -> bool {
        true
    }

    fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let controller = self.controller.clone();
        Box::pin(async move {
            let args: ExitPlanModeInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            // 1. 校验当前为 Plan 模式（C-25：plan.exit 仅 Plan 模式可调）
            let snap = controller.snapshot().await;
            if snap.mode != PermissionMode::Plan {
                return Err(ToolError::InvalidStateTransition(format!(
                    "plan.exit 仅在 Plan 模式下可调用（当前：{:?}）",
                    snap.mode
                )));
            }

            // 2. 切换模式 + 缓存 allowed_prompts
            let target = args.target_mode.to_permission_mode();
            controller
                .exit_plan(args.allowed_prompts.clone(), target)
                .await
                .map_err(|e| ToolError::Exec(format!("exit_plan failed: {e}")))?;

            // 3. 返回结果（提示用户决策门已触发，模型应等待用户响应）
            Ok(ToolResult::ok_json(json!({
                "status": "plan_exited",
                "plan_path": args.plan_path,
                "target_mode": match args.target_mode {
                    TargetMode::Default => "default",
                    TargetMode::AcceptEdits => "accept_edits",
                },
                "allowed_prompts_count": args.allowed_prompts.len(),
                "plan_was_edited": args.plan_was_edited,
                "hint": "已切换到执行期，用户可在新模式下继续对话。预批准命令将自动 Allow。",
            })))
        })
    }
}

#[cfg(test)]
mod tests {
    //! `plan.exit` 工具测试：状态机 + 预批准缓存 + Plan 模式守卫。

    use super::*;
    use minicoding_core::policy::PlanModeSnapshot;
    use tokio::sync::RwLock;

    /// 测试用 controller：直接持有 `RwLock<PlanModeSnapshot>`。
    struct TestController {
        state: Arc<RwLock<PlanModeSnapshot>>,
    }

    impl PlanModeController for TestController {
        fn snapshot(&self) -> BoxFuture<'_, PlanModeSnapshot> {
            let state = self.state.clone();
            Box::pin(async move { state.read().await.clone() })
        }

        fn exit_plan(
            &self,
            allowed_prompts: Vec<PreApprovedPrompt>,
            target_mode: PermissionMode,
        ) -> BoxFuture<'_, Result<(), minicoding_core::model::PolicyError>> {
            let state = self.state.clone();
            Box::pin(async move {
                let mut snap = state.write().await;
                if snap.mode != PermissionMode::Plan {
                    return Err(minicoding_core::model::PolicyError::Policy(format!(
                        "not in Plan mode (current: {:?})",
                        snap.mode
                    )));
                }
                snap.mode = target_mode;
                snap.allowed_prompts = allowed_prompts;
                Ok(())
            })
        }

        fn set_mode(&self, mode: PermissionMode) -> BoxFuture<'_, ()> {
            let state = self.state.clone();
            Box::pin(async move {
                state.write().await.mode = mode;
            })
        }
    }

    fn make_controller(mode: PermissionMode) -> Arc<TestController> {
        Arc::new(TestController {
            state: Arc::new(RwLock::new(PlanModeSnapshot {
                mode,
                allowed_prompts: Vec::new(),
            })),
        })
    }

    fn make_ctx() -> ToolContext {
        ToolContext::new("/tmp/proj".into(), "test".to_string())
    }

    #[tokio::test]
    async fn plan_exit_switches_to_default_and_caches_prompts() {
        let controller = make_controller(PermissionMode::Plan);
        let tool = PlanExit::new(controller.clone());
        let input = json!({
            "plan_path": ".minicoding/plan.md",
            "allowed_prompts": [
                {"tool": "shell.run", "prompt": "cargo build"},
                {"tool": "shell.run", "prompt": "cargo test"}
            ],
            "target_mode": "default"
        });
        let result = tool.execute(input, &make_ctx()).await.unwrap();
        assert!(!result.is_error);
        let snap = controller.snapshot().await;
        assert_eq!(snap.mode, PermissionMode::Default);
        assert_eq!(snap.allowed_prompts.len(), 2);
    }

    #[tokio::test]
    async fn plan_exit_switches_to_accept_edits() {
        let controller = make_controller(PermissionMode::Plan);
        let tool = PlanExit::new(controller.clone());
        let input = json!({
            "plan_path": ".minicoding/plan.md",
            "target_mode": "accept_edits"
        });
        tool.execute(input, &make_ctx()).await.unwrap();
        assert_eq!(
            controller.snapshot().await.mode,
            PermissionMode::AcceptEdits
        );
    }

    #[tokio::test]
    async fn plan_exit_rejects_when_not_in_plan_mode() {
        let controller = make_controller(PermissionMode::Default);
        let tool = PlanExit::new(controller.clone());
        let input = json!({
            "plan_path": ".minicoding/plan.md",
        });
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidStateTransition(_)));
        // 模式未变
        assert_eq!(controller.snapshot().await.mode, PermissionMode::Default);
    }

    #[tokio::test]
    async fn plan_exit_default_target_mode_is_default() {
        let controller = make_controller(PermissionMode::Plan);
        let tool = PlanExit::new(controller.clone());
        // 省略 target_mode
        let input = json!({"plan_path": ".minicoding/plan.md"});
        tool.execute(input, &make_ctx()).await.unwrap();
        assert_eq!(controller.snapshot().await.mode, PermissionMode::Default);
    }

    #[tokio::test]
    async fn plan_exit_empty_allowed_prompts_works() {
        let controller = make_controller(PermissionMode::Plan);
        let tool = PlanExit::new(controller.clone());
        let input = json!({"plan_path": ".minicoding/plan.md"});
        let result = tool.execute(input, &make_ctx()).await.unwrap();
        assert!(!result.is_error);
        assert!(controller.snapshot().await.allowed_prompts.is_empty());
    }

    #[tokio::test]
    async fn plan_exit_invalid_input_returns_error() {
        let controller = make_controller(PermissionMode::Plan);
        let tool = PlanExit::new(controller.clone());
        // 缺少必填的 plan_path
        let input = json!({"target_mode": "default"});
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn plan_exit_is_read_only_and_no_side_effect() {
        let controller = make_controller(PermissionMode::Plan);
        let tool = PlanExit::new(controller);
        assert_eq!(tool.side_effect(), SideEffect::None);
        assert!(tool.is_read_only());
    }
}
