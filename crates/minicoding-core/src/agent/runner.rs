//! `SubagentRunner` trait + `NoopSubagentRunner`（见 `design.md` §7.3）。
//!
//! Runtime 持有 `Arc<dyn SubagentRunner>`，`task.spawn` 工具通过它派发子 Agent。
//! 默认 `NoopSubagentRunner` 返回 `NotConfigured` 错误——未启用子 Agent feature
//! 或未注入实现时，`task.spawn` 调用直接失败（不静默 no-op，避免模型误以为已派发）。
//!
//! ## `OTel` span 传播
//!
//! 实现者需在 `spawn` 内开 `tracing::info_span!("subagent", ty = %spec.ty.as_str(),
//! subagent.id = %task_id)`，并通过 `tracing` 的 span context 自动挂在父 turn span
//! 下（design.md §15.2）。M5 不引入显式 Context 传播 API——`tracing` 的
//! `info_span!` 已通过当前线程的 `Span::current()` 建立父子关系，调用方在 turn
//! span 内调用 `task.spawn` 即可形成层级（C-13 `OTel` 一等公民）。

use crate::model::{RuntimeError, SubagentResult, SubagentSpec};
use crate::provider::BoxFuture;

/// 子 Agent 派发器（`dyn` 兼容）。
///
/// 实现者负责：
/// 1. 创建独立 `ContextManager` 与 `messages`（隔离上下文）；
/// 2. 按 `spec.ty` 选择系统提示词与工具子集（`Explore` 强制小模型 + 只读工具）；
/// 3. 跑完整 Agent 循环（共享父会话 `ToolRegistry`/`Storage`/`PermissionPolicy`/
///    `SandboxDriver`，但 `max_iters` 更小、超时更短）；
/// 4. 返回 `SubagentResult`（仅 `summary`，不回灌中间消息，C-05）。
///
/// # Errors
/// - `RuntimeError::Config`：子 Agent 未启用或 runner 未注入；
/// - `RuntimeError::Llm`：子 Agent 调 LLM 失败；
/// - `RuntimeError::Interrupted`：子 Agent 被取消（Ctrl-C 或父 turn 取消传播）。
pub trait SubagentRunner: Send + Sync {
    /// 派发子 Agent 并等待结果（同步等待，简化模型——并行 map-reduce 见 §7.4，MVP 不交付）。
    ///
    /// `input` 是父 Agent 给子 Agent 的任务描述（自然语言），由 `task.spawn`
    /// 工具入参 `prompt` 字段传入。
    fn spawn(
        &self,
        spec: SubagentSpec,
        input: String,
    ) -> BoxFuture<'_, Result<SubagentResult, RuntimeError>>;
}

/// 无操作子 Agent runner（兜底，未注入实现时使用）。
///
/// `spawn` 恒返回 `RuntimeError::Config`——`task.spawn` 调用直接失败，避免模型
/// 误以为已派发。真实场景应由 frontend 注入 `InProcessSubagentRunner` 或外部实现。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSubagentRunner;

impl NoopSubagentRunner {
    /// 创建兜底 runner。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl SubagentRunner for NoopSubagentRunner {
    fn spawn(
        &self,
        spec: SubagentSpec,
        _input: String,
    ) -> BoxFuture<'_, Result<SubagentResult, RuntimeError>> {
        let ty = spec.ty;
        Box::pin(async move {
            Err(RuntimeError::Config(format!(
                "subagent runner not configured (ty = {})",
                ty.as_str()
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    //! `NoopSubagentRunner` 兜底测试 + trait `dyn` 兼容性验证。

    use super::*;
    use crate::model::SubagentType;
    use std::sync::Arc;

    #[tokio::test]
    async fn noop_runner_returns_config_error() {
        let runner = NoopSubagentRunner::new();
        let spec = SubagentSpec::default_for(SubagentType::Explore);
        let err = runner
            .spawn(spec, "find foo".to_string())
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Config(_)));
    }

    #[tokio::test]
    async fn trait_is_dyn_compatible() {
        // 验证 `dyn SubagentRunner` 可构造（trait object 兼容性回归）。
        let runner: Arc<dyn SubagentRunner> = Arc::new(NoopSubagentRunner::new());
        let spec = SubagentSpec::default_for(SubagentType::Plan);
        let result = runner.spawn(spec, "demo".to_string()).await;
        assert!(result.is_err());
    }
}
