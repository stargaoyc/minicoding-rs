//! `ui.ask` 工具组：LLM 主动向用户提问（2026-08-23 审查 §6-P2 补齐，
//! 对标 Claude Code 的 `AskUserQuestion`）。
//!
//! 走 [`minicoding_core::policy::PermissionPrompter`] 点对点通道（架构 §3.9：
//! 决策与交互分离的既有设施），并广播 `PermissionRequested`/`PermissionResolved`
//! 事件——与 Runtime 权限链同一 UX 通路（TUI 弹窗/Web Dialog/SSE 推送）。
//!
//! ## v1 边界（有意为之）
//!
//! 二值问答（同意/拒绝）。`PermissionPrompt.options` 当前是固定枚举
//! （`PromptOption`），多选自定义选项需扩展协议并贯通四端前端——列为后续项。
//!
//! ## 已知限制
//!
//! 经工具路径触发的提问**不落 `audit.log`、不持久化 `PermissionResolved` 事件**
//! （那是 Runtime 权限链的职责）；问答结果由 LLM 转述进对话消息，随会话落盘。

use minicoding_core::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use minicoding_core::policy::{Decision, PermissionPrompt, PromptOption};
use minicoding_core::provider::BoxFuture;
use minicoding_core::runtime::{Event, EventBus};
use minicoding_core::tool::{RenderIntent, Tool, ToolContext};

/// LLM 主动向用户提问的工具（二值：同意/拒绝）。
pub struct UiAsk {
    schema: ToolSchema,
}

impl UiAsk {
    /// 创建 `ui.ask` 工具实例。
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            name: "ui.ask".into(),
            description: "向用户提出是/否问题并等待回答。用于需要用户决策的歧义场景\
                          （如多种实现方案取舍、是否执行高风险操作）。返回 yes 或 no。"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "要问用户的问题（应自包含上下文，用户看不到对话历史）。"
                    }
                },
                "required": ["question"]
            }),
        };
        Self { schema }
    }

    /// 广播事件（权限链同款语义；无 `EventBus` 注入时静默跳过——兼容测试）。
    fn emit(events: Option<&EventBus>, event: Event) {
        if let Some(bus) = events {
            bus.emit(event);
        }
    }
}

impl Default for UiAsk {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for UiAsk {
    fn name(&self) -> &str {
        &self.schema.name
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    /// 提问本身无副作用（不写文件/不执行命令/不联网）；阻塞等待用户回答
    /// 由 turn 级超时兜底。归入只读桶意味着可与其它只读工具并行——各提问
    /// 有独立 prompt id，前端按序展示，可接受。
    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    fn execute(
        &self,
        params: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let question = match params.get("question").and_then(|v| v.as_str()) {
            Some(q) => q.to_string(),
            None => {
                return Box::pin(async { Err(ToolError::InvalidInput("question 缺失".into())) });
            }
        };
        let Some(prompter) = ctx.prompter.clone() else {
            return Box::pin(async {
                Ok(ToolResult::err_text(
                    "当前环境无交互通道（非交互运行），无法向用户提问；请自行做出合理假设并在回复中说明",
                ))
            });
        };
        let events = ctx.events.clone();
        Box::pin(async move {
            let prompt = PermissionPrompt {
                id: format!("ask-{}", ulid::Ulid::new()),
                tool: "ui.ask".to_string(),
                summary: question,
                risk: minicoding_core::policy::Risk::Low,
                options: vec![PromptOption::AllowOnce, PromptOption::DenyOnce],
            };

            Self::emit(
                events.as_ref(),
                Event::PermissionRequested {
                    id: prompt.id.clone(),
                    tool: prompt.tool.clone(),
                    summary: prompt.summary.clone(),
                    risk: prompt.risk,
                },
            );
            let decision = prompter.prompt(prompt.clone()).await;
            Self::emit(
                events.as_ref(),
                Event::PermissionResolved {
                    id: prompt.id.clone(),
                    decision: decision.clone(),
                },
            );
            let answer = match decision {
                Decision::Allow => "yes".to_string(),
                Decision::Deny(reason) if reason.is_empty() => "no".to_string(),
                Decision::Deny(reason) => format!("no ({reason})"),
            };
            Ok(ToolResult::ok_text(format!("用户回答: {answer}")))
        })
    }

    /// 渲染意图（R-05，M-11）：问答确认，文本直出。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        RenderIntent::default_for(result)
    }
}

use minicoding_core::tool::ToolRegistry;

/// 注册 `ui.ask` 到 registry（只读桶；需 `ToolContext` 注入 prompter/events 才有实际交互能力）。
pub fn register_ui_tools(registry: &mut ToolRegistry) {
    registry.register(std::sync::Arc::new(UiAsk::new()));
}
