//! `task.spawn`：派发类型化子 Agent（见 `design.md` §7.3）。
//!
//! 父 Agent 通过该工具派发 `Explore`/`Plan`/`GeneralPurpose`/`Custom` 子 Agent，
//! 隔离上下文执行子任务，仅回收 `summary`（C-05：子 Agent 上下文是数据非指令）。
//!
//! ## 设计要点
//!
//! - `SideEffect::None`：父 Agent 只接收 `summary`，子 Agent 的副作用在其自身
//!   权限检查中处理（不重复过父会话权限链）；
//! - 持有 `Arc<dyn SubagentRunner>` 反向调用 Runtime 派发（与 `plan.exit` 持有
//!   `Arc<dyn PlanModeController>` 同构，避免 core 依赖 tools）；
//! - 持有 `Arc<dyn PlanModeController>` 实现 Plan 模式守卫：`SubagentType::Plan`
//!   仅在 `PermissionMode::Plan` 下可派发，其它模式下退化为 `Explore`（design.md §7.3）；
//! - 不嵌套约束：`spec.can_spawn_subagent == false` 时由 runner 在子 Agent 工具集中
//!   移除 `task.spawn`（design.md §7.3）；工具层另有深度防御（T-4，2026-08-25 审查）：
//!   子 Agent Runtime 组装时经 [`TaskSpawn::with_can_spawn_subagent`] 传入
//!   `spec.can_spawn_subagent`，为 `false` 时工具直接拒绝派发——不依赖 runner
//!   移除工具这一单一防线。

use minicoding_core::agent::SubagentRunner;
use minicoding_core::model::{
    SideEffect, SubagentSpec, SubagentType, Thoroughness, ToolError, ToolResult, ToolSchema,
};
use minicoding_core::policy::PlanModeController;
use minicoding_core::provider::BoxFuture;
use minicoding_core::tool::{RenderIntent, Tool, ToolContext, ToolOutputSchema};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 派发子 Agent 的工具（`SideEffect::None`）。
///
/// 父 Agent 只接收 `summary`，子 Agent 内部的副作用由其自身权限链处理。
pub struct TaskSpawn {
    schema: ToolSchema,
    output_schema: ToolOutputSchema,
    runner: Arc<dyn SubagentRunner>,
    plan_controller: Arc<dyn PlanModeController>,
    /// 本 Runtime 是否允许派发子 Agent（镜像所属 spec 的 `can_spawn_subagent`，
    /// T-4 深度防御；顶层 Runtime 注册点恒为 `true`）。
    can_spawn: bool,
}

impl TaskSpawn {
    /// 创建 `task.spawn` 工具实例。
    ///
    /// - `runner`：由 `Runtime::subagent_runner()` 提供；
    /// - `plan_controller`：由 `Runtime::plan_controller()` 提供，用于 Plan 模式守卫。
    #[must_use]
    pub fn new(
        runner: Arc<dyn SubagentRunner>,
        plan_controller: Arc<dyn PlanModeController>,
    ) -> Self {
        let schema = ToolSchema {
            name: "task.spawn".to_string(),
            description: "派发类型化子 Agent 隔离上下文执行子任务（见 design.md §7）。\
父 Agent 只接收 summary，子 Agent 上下文不回灌（C-05）。\n\
- `explore`：快速代码库探查（小模型 + 只读工具，跳过 AGENTS.md）；\n\
- `plan`：仅 Plan 模式可派发，其它模式退化为 explore；\n\
- `general`：继承父会话模型与全工具，复杂多步任务；\n\
- `custom`：从 .minicoding/agents/*.md 加载（frontmatter 配置）。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "type": {
                        "type": "string",
                        "enum": ["explore", "plan", "general", "custom"],
                        "description": "子 Agent 类型（默认 explore）。"
                    },
                    "custom_name": {
                        "type": "string",
                        "description": "type=custom 时必填：.minicoding/agents/<name>.md 的 stem。"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "给子 Agent 的任务描述（自然语言）。"
                    },
                    "thoroughness": {
                        "type": "string",
                        "enum": ["quick", "medium", "very_thorough"],
                        "description": "探查彻底度（仅 explore 用，默认 medium）。"
                    },
                    "model": {
                        "type": "string",
                        "description": "覆盖模型 ID（None = 继承父会话；explore 强制小模型由 runner 解析）。"
                    },
                    "max_iters": {
                        "type": "integer",
                        "description": "覆盖最大迭代轮次（默认按类型）。"
                    }
                },
                "required": ["prompt"]
            }),
        };
        // R-05（M-11）：声明输出 JSON 形态（summary + artifacts + 成本/完成标记）。
        let output_schema = ToolOutputSchema {
            schema: json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string"},
                    "artifacts": {"type": "array"},
                    "token_used": {"type": "integer"},
                    "completed": {"type": "boolean"}
                },
                "required": ["summary"]
            }),
        };
        Self {
            schema,
            output_schema,
            runner,
            plan_controller,
            can_spawn: true,
        }
    }

    /// 设置本工具实例是否允许派发（T-4 深度防御，2026-08-25 审查）。
    ///
    /// 子 Agent Runtime 组装时应传 `spec.can_spawn_subagent`：为 `false` 时
    /// `execute` 直接返回错误结果，不调用 runner——即便 runner 未从子 Agent
    /// 工具集中移除本工具，嵌套派发也不可绕过。
    #[must_use]
    pub fn with_can_spawn_subagent(mut self, allowed: bool) -> Self {
        self.can_spawn = allowed;
        self
    }
}

/// `task.spawn` 工具入参（见 `design.md` §7.2）。
#[derive(Debug, Deserialize)]
struct SpawnInput {
    /// 子 Agent 类型（默认 `explore`）。
    #[serde(default, rename = "type")]
    ty: SpawnTypeSerde,
    /// `type=custom` 时必填。
    custom_name: Option<String>,
    /// 任务描述。
    prompt: String,
    /// 探查彻底度（仅 explore 用）。
    thoroughness: Option<ThoroughnessSerde>,
    /// 覆盖模型 ID。
    model: Option<String>,
    /// 覆盖最大迭代轮次。
    max_iters: Option<u32>,
}

/// `type` 字段反序列化辅助（kebab-case 与 design.md 表格一致）。
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SpawnTypeSerde {
    #[default]
    Explore,
    Plan,
    General,
    Custom,
}

impl SpawnTypeSerde {
    fn into_subagent_type(self, custom_name: Option<String>) -> Result<SubagentType, ToolError> {
        match self {
            Self::Explore => Ok(SubagentType::Explore),
            Self::Plan => Ok(SubagentType::Plan),
            Self::General => Ok(SubagentType::GeneralPurpose),
            Self::Custom => {
                let name = custom_name.ok_or_else(|| {
                    ToolError::InvalidInput("custom_name required for type=custom".to_string())
                })?;
                if name.trim().is_empty() {
                    return Err(ToolError::InvalidInput(
                        "custom_name must not be empty".to_string(),
                    ));
                }
                Ok(SubagentType::Custom(name))
            }
        }
    }
}

/// `thoroughness` 字段反序列化辅助。
#[derive(Debug, Copy, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ThoroughnessSerde {
    Quick,
    Medium,
    VeryThorough,
}

impl ThoroughnessSerde {
    const fn into_thoroughness(self) -> Thoroughness {
        match self {
            Self::Quick => Thoroughness::Quick,
            Self::Medium => Thoroughness::Medium,
            Self::VeryThorough => Thoroughness::VeryThorough,
        }
    }
}

impl Tool for TaskSpawn {
    fn name(&self) -> &'static str {
        "task.spawn"
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let runner = self.runner.clone();
        let plan_controller = self.plan_controller.clone();
        // TL-R6-3：子代理摘要截断上限（async 块外捕获，闭包 move）
        let max_out = ctx.max_output_bytes;
        Box::pin(async move {
            // T-4 深度防御（2026-08-25 审查）：所属 spec 禁止再生子 Agent 时，
            // 工具层直接拒绝（is_error=true 的错误结果），不依赖 runner 移除工具。
            if !self.can_spawn {
                return Ok(ToolResult::err_text(
                    "task.spawn 在当前上下文中被禁用（can_spawn_subagent=false，不允许嵌套派发子 Agent）",
                ));
            }

            let args: SpawnInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(e.to_string()))?;

            let mut ty = args.ty.into_subagent_type(args.custom_name)?;

            // Plan 模式守卫（design.md §7.3）：SubagentType::Plan 仅在 Plan 模式下可派发，
            // 其它模式下退化为 Explore（不报错，避免模型因模式状态失败重试）。
            let snap = plan_controller.snapshot().await;
            if matches!(ty, SubagentType::Plan)
                && snap.mode != minicoding_core::policy::PermissionMode::Plan
            {
                tracing::info!(
                    current_mode = ?snap.mode,
                    "plan subagent requested outside Plan mode; degrading to Explore"
                );
                ty = SubagentType::Explore;
            }

            let mut spec = SubagentSpec::default_for(ty);
            if let Some(t) = args.thoroughness {
                spec.thoroughness = t.into_thoroughness();
                // thoroughness 覆盖时同步刷新 max_iters 默认值（仅 Explore 有意义）
                if matches!(spec.ty, SubagentType::Explore) {
                    spec.max_iters = spec.thoroughness.default_max_iters();
                }
            }
            if let Some(m) = args.model {
                spec.model = Some(m);
            }
            if let Some(iters) = args.max_iters {
                if iters == 0 {
                    return Err(ToolError::InvalidInput("max_iters must be > 0".to_string()));
                }
                spec.max_iters = iters;
            }

            // OTel span：subagent 派发挂在父 turn span 下（design.md §15.2）。
            // 字段不含 prompt 原文（C-04：避免任务描述中可能含的凭证外泄）。
            let span = tracing::info_span!(
                "subagent",
                ty = %spec.ty.as_str(),
                max_iters = spec.max_iters,
                otel.name = "subagent",
            );
            let _enter = span.enter();

            let result = runner
                .spawn(spec, args.prompt)
                .await
                .map_err(|e| ToolError::Exec(format!("subagent spawn failed: {e}")))?;

            // TL-R6-3（2026-08-28 R6 审查）：子代理摘要直入父 Agent 上下文——
            // 此前无截断，子代理可产出任意长度摘要挤占父代理全部上下文预算
            //（C-07 资源上限）。与 `fs.read`/`shell.run` 等读工具同口径截断。
            let (summary, truncated) = crate::util::truncate_output(result.summary, max_out);
            let mut json = json!({
                "summary": summary,
                "artifacts": result.artifacts,
                "token_used": result.token_used,
                "completed": result.completed,
            });
            if truncated {
                json["summary_truncated"] = json!(true);
            }
            Ok(ToolResult::ok_json(json))
        })
    }

    /// 输出 JSON Schema（R-05，M-11）：`summary` + `artifacts` + `token_used` + `completed`。
    fn output_schema(&self) -> Option<&ToolOutputSchema> {
        Some(&self.output_schema)
    }

    /// 渲染意图（R-05，M-11）：结构化 JSON 直出。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        RenderIntent::default_for(result)
    }
}

/// 注册 `task.spawn` 工具（仅注册该工具，task 工具组其它工具由 `register_task_tools` 注册）。
///
/// 调用方需同时调用 `register_task_tools` 注册其它 task 工具。
pub fn register_spawn_tool(
    registry: &mut minicoding_core::tool::ToolRegistry,
    runner: Arc<dyn SubagentRunner>,
    plan_controller: Arc<dyn PlanModeController>,
) {
    registry.register(Arc::new(TaskSpawn::new(runner, plan_controller)));
}

#[cfg(test)]
mod tests {
    //! `task.spawn` 工具测试：入参解析、Plan 模式守卫、runner 调用、错误传播。

    use super::*;
    use minicoding_core::model::{RuntimeError, SubagentResult};
    use minicoding_core::policy::{PermissionMode, PlanModeSnapshot};
    use tokio::sync::RwLock;

    /// 测试用 runner：返回固定结果或错误，便于断言。
    struct MockRunner {
        captured: Arc<RwLock<Option<SubagentSpec>>>,
        result: Arc<tokio::sync::Mutex<Option<Result<SubagentResult, RuntimeError>>>>,
    }

    impl MockRunner {
        fn with_ok(summary: &str) -> Arc<Self> {
            Arc::new(Self {
                captured: Arc::new(RwLock::new(None)),
                result: Arc::new(tokio::sync::Mutex::new(Some(Ok(
                    SubagentResult::completed(summary.to_string(), 42),
                )))),
            })
        }
        fn with_err(err: RuntimeError) -> Arc<Self> {
            Arc::new(Self {
                captured: Arc::new(RwLock::new(None)),
                result: Arc::new(tokio::sync::Mutex::new(Some(Err(err)))),
            })
        }
    }

    impl SubagentRunner for MockRunner {
        fn spawn(
            &self,
            spec: SubagentSpec,
            input: String,
        ) -> BoxFuture<'_, Result<SubagentResult, RuntimeError>> {
            let captured = self.captured.clone();
            let result = self.result.clone();
            Box::pin(async move {
                *captured.write().await = Some(spec.clone());
                let _ = input; // 不在断言中使用
                // 取出结果（一次性），构造错误信息时附上 ty
                let r = result.lock().await.take();
                match r {
                    Some(Ok(res)) => Ok(res),
                    Some(Err(RuntimeError::Config(_))) => Err(RuntimeError::Config(format!(
                        "{} triggered",
                        spec.ty.as_str()
                    ))),
                    Some(Err(other)) => Err(other),
                    None => Ok(SubagentResult::completed(
                        "stale mock result".to_string(),
                        0,
                    )),
                }
            })
        }
    }

    /// 测试用 controller：可设置初始 mode。
    struct MockController {
        state: Arc<RwLock<PlanModeSnapshot>>,
    }

    impl MockController {
        fn new(mode: PermissionMode) -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(RwLock::new(PlanModeSnapshot {
                    mode,
                    allowed_prompts: Vec::new(),
                })),
            })
        }
    }

    impl PlanModeController for MockController {
        fn snapshot(&self) -> BoxFuture<'_, PlanModeSnapshot> {
            let state = self.state.clone();
            Box::pin(async move { state.read().await.clone() })
        }
        fn exit_plan(
            &self,
            _allowed: Vec<minicoding_core::policy::PreApprovedPrompt>,
            _target: PermissionMode,
        ) -> BoxFuture<'_, Result<(), minicoding_core::model::PolicyError>> {
            Box::pin(async { Ok(()) })
        }
        fn set_mode(&self, _mode: PermissionMode) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    fn make_ctx() -> ToolContext {
        ToolContext::new("/tmp/proj".into(), "test".to_string())
    }

    #[tokio::test]
    async fn spawn_explore_calls_runner_and_returns_summary() {
        let runner = MockRunner::with_ok("found foo in src/lib.rs:42");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner.clone(), controller);
        let input = json!({
            "type": "explore",
            "prompt": "find foo"
        });
        let result = tool.execute(input, &make_ctx()).await.unwrap();
        assert!(!result.is_error);
        let captured = runner.captured.read().await.clone().unwrap();
        assert_eq!(captured.ty, SubagentType::Explore);
    }

    #[tokio::test]
    async fn spawn_plan_outside_plan_mode_degrades_to_explore() {
        let runner = MockRunner::with_ok("explored");
        let controller = MockController::new(PermissionMode::Default); // 非 Plan
        let tool = TaskSpawn::new(runner.clone(), controller);
        let input = json!({"type": "plan", "prompt": "collect context"});
        tool.execute(input, &make_ctx()).await.unwrap();
        let captured = runner.captured.read().await.clone().unwrap();
        assert_eq!(captured.ty, SubagentType::Explore); // 退化为 Explore
    }

    #[tokio::test]
    async fn spawn_plan_in_plan_mode_stays_plan() {
        let runner = MockRunner::with_ok("planned");
        let controller = MockController::new(PermissionMode::Plan);
        let tool = TaskSpawn::new(runner.clone(), controller);
        let input = json!({"type": "plan", "prompt": "collect context"});
        tool.execute(input, &make_ctx()).await.unwrap();
        let captured = runner.captured.read().await.clone().unwrap();
        assert_eq!(captured.ty, SubagentType::Plan);
    }

    #[tokio::test]
    async fn spawn_custom_without_name_returns_invalid_input() {
        let runner = MockRunner::with_ok("ok");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner, controller);
        let input = json!({"type": "custom", "prompt": "do thing"});
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn spawn_custom_with_name_passes_through() {
        let runner = MockRunner::with_ok("done");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner.clone(), controller);
        let input = json!({
            "type": "custom",
            "custom_name": "reviewer",
            "prompt": "review PR"
        });
        tool.execute(input, &make_ctx()).await.unwrap();
        let captured = runner.captured.read().await.clone().unwrap();
        assert_eq!(captured.ty, SubagentType::Custom("reviewer".to_string()));
    }

    #[tokio::test]
    async fn spawn_thoroughness_overrides_max_iters_for_explore() {
        let runner = MockRunner::with_ok("ok");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner.clone(), controller);
        let input = json!({
            "type": "explore",
            "prompt": "find",
            "thoroughness": "very_thorough"
        });
        tool.execute(input, &make_ctx()).await.unwrap();
        let captured = runner.captured.read().await.clone().unwrap();
        assert_eq!(captured.thoroughness, Thoroughness::VeryThorough);
        assert_eq!(
            captured.max_iters,
            Thoroughness::VeryThorough.default_max_iters()
        );
    }

    #[tokio::test]
    async fn spawn_max_iters_override_works() {
        let runner = MockRunner::with_ok("ok");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner.clone(), controller);
        let input = json!({
            "type": "explore",
            "prompt": "x",
            "max_iters": 5
        });
        tool.execute(input, &make_ctx()).await.unwrap();
        let captured = runner.captured.read().await.clone().unwrap();
        assert_eq!(captured.max_iters, 5);
    }

    #[tokio::test]
    async fn spawn_max_iters_zero_rejected() {
        let runner = MockRunner::with_ok("ok");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner, controller);
        let input = json!({"type": "explore", "prompt": "x", "max_iters": 0});
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn spawn_runner_error_propagates_as_exec_error() {
        let runner = MockRunner::with_err(RuntimeError::Config("no runner".to_string()));
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner, controller);
        let input = json!({"type": "explore", "prompt": "x"});
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Exec(_)));
    }

    #[tokio::test]
    async fn spawn_denied_when_can_spawn_false_deep_defense() {
        // T-4（2026-08-25 审查）：can_spawn_subagent=false 时工具层直接返回
        // 错误结果，runner 不被调用（不依赖 runner 移除工具）。
        let runner = MockRunner::with_ok("should not run");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner.clone(), controller).with_can_spawn_subagent(false);
        let result = tool
            .execute(json!({"type": "explore", "prompt": "x"}), &make_ctx())
            .await
            .expect("应返回错误结果而非 Err");
        assert!(result.is_error, "拒绝派发应标记 is_error=true");
        match result.content {
            minicoding_core::model::ToolContent::Text(t) => {
                assert!(t.contains("can_spawn_subagent"), "错误文本应说明原因: {t}");
            }
            other => panic!("expected text content, got {other:?}"),
        }
        assert!(runner.captured.read().await.is_none(), "runner 不应被调用");
    }

    #[tokio::test]
    async fn spawn_missing_prompt_rejected() {
        let runner = MockRunner::with_ok("ok");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner, controller);
        let input = json!({"type": "explore"});
        let err = tool.execute(input, &make_ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn spawn_default_type_is_explore() {
        let runner = MockRunner::with_ok("ok");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner.clone(), controller);
        // 省略 type
        let input = json!({"prompt": "do"});
        tool.execute(input, &make_ctx()).await.unwrap();
        let captured = runner.captured.read().await.clone().unwrap();
        assert_eq!(captured.ty, SubagentType::Explore);
    }

    #[test]
    fn task_spawn_is_read_only_and_no_side_effect() {
        let runner = MockRunner::with_ok("ok");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner, controller);
        assert_eq!(tool.side_effect(), SideEffect::None);
        assert!(tool.is_read_only());
    }

    #[test]
    fn spawn_declares_output_schema() {
        // R-05（M-11）：task.spawn 声明输出 JSON 形态（summary + artifacts + 成本）。
        let runner = MockRunner::with_ok("ok");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner, controller);
        let schema = tool.output_schema().expect("output schema");
        assert_eq!(schema.schema["type"], "object");
        assert_eq!(schema.schema["properties"]["summary"]["type"], "string");
        assert!(
            schema.schema["required"]
                .as_array()
                .expect("required")
                .iter()
                .any(|v| v == "summary")
        );
    }

    #[test]
    fn spawn_render_output_defaults_to_json() {
        let runner = MockRunner::with_ok("ok");
        let controller = MockController::new(PermissionMode::Default);
        let tool = TaskSpawn::new(runner, controller);
        let result = ToolResult::ok_json(json!({
            "summary": "done",
            "artifacts": [],
            "token_used": 10,
            "completed": true
        }));
        match tool.render_output(&result) {
            RenderIntent::Json { .. } => {}
            other => panic!("expected Json, got {other:?}"),
        }
    }
}
