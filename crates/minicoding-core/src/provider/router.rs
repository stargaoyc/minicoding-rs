//! 模型路由骨架（T-M6-5，对应 L-06）。
//!
//! `Router` trait 按任务/上下文选择 provider，支持"规划用大模型、补全用小模型"等场景
//! （见 `design.md` §5.4）。M6 仅交付骨架 + `StaticRouter`（恒返回同一 provider）；
//! 真正按任务路由的实现留给 M7+（需 `Task`/`ContextSnapshot` 更完整的字段）。
//!
//! ## 设计要点
//!
//! - **C-13 防死循环**：`Router::pick` 是无状态纯选择，不涉及重试逻辑；重试由
//!   `RetryProvider` 装饰器负责（bounded `max_retries`，见 `common::retry`）。
//! - **`dyn` 兼容**：trait 方法返回 `Arc<dyn LlmProvider>`，Runtime 持有
//!   `Arc<dyn Router>` 即可切换路由策略不需改签名。
//! - **`StaticRouter`**：M6 兜底实现，始终返回构造时传入的 provider。等价于"无路由"。

use std::sync::Arc;

use crate::provider::LlmProvider;

/// 模型路由 trait（`dyn` 兼容，见 `design.md` §5.4）。
///
/// 实现者根据任务类型/上下文快照选择最合适的 provider。M6 仅 `StaticRouter`；
/// M7+ 可实现 `TaskBasedRouter`（按 `Task::kind`）、`CostAwareRouter`（按 token 预算）等。
///
/// ## C-13 约束
///
/// `pick` 仅做选择，不做重试。错误恢复由 `RetryProvider` 装饰器负责，重试次数有上限
/// （`RetryConfig::max_retries`），不会无限重试导致死循环。
pub trait Router: Send + Sync {
    /// 选择 provider。
    ///
    /// `task_kind` 为任务类型 hint（如 `"plan"`/`"code"`/`"summary"`），`None` 表示无 hint，
    /// 路由器应回退到默认 provider。
    fn pick(&self, task_kind: Option<&str>) -> Arc<dyn LlmProvider>;
}

/// 静态路由：始终返回同一 provider（M6 兜底实现）。
///
/// 等价于"无路由"——所有任务用同一 provider。M7+ 引入 `TaskBasedRouter` 后，
/// `StaticRouter` 保留作为配置未指定路由策略时的默认值。
pub struct StaticRouter {
    provider: Arc<dyn LlmProvider>,
}

impl StaticRouter {
    /// 构造静态路由器。
    #[must_use]
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self { provider }
    }
}

impl Router for StaticRouter {
    fn pick(&self, _task_kind: Option<&str>) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LlmError;
    use crate::provider::{BoxFuture, BoxStream, Capabilities, ChatRequest, Tokenizer};

    /// 测试用 stub provider（仅实现 trait 最小接口）。
    struct StubProvider {
        id_str: &'static str,
    }

    impl LlmProvider for StubProvider {
        fn id(&self) -> &str {
            self.id_str
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_tool_call: false,
                supports_vision: false,
                supports_streaming: false,
                supports_json_mode: false,
                context_window: 1_000,
                max_output: 1_000,
            }
        }
        fn tokenizer(&self) -> Arc<dyn Tokenizer> {
            Arc::new(StubTokenizer)
        }
        fn chat_stream(
            &self,
            _req: ChatRequest,
        ) -> BoxFuture<
            '_,
            Result<BoxStream<'static, Result<crate::provider::Delta, LlmError>>, LlmError>,
        > {
            Box::pin(async { Err(LlmError::NotConfigured) })
        }
        fn count_tokens(&self, _messages: &[crate::model::Message]) -> BoxFuture<'_, usize> {
            Box::pin(async { 0 })
        }
    }

    struct StubTokenizer;

    impl Tokenizer for StubTokenizer {
        fn count(&self, text: &str) -> usize {
            text.len()
        }
        fn count_messages(&self, msgs: &[crate::model::Message]) -> usize {
            msgs.len()
        }
        fn id(&self) -> &'static str {
            "stub"
        }
    }

    #[test]
    fn static_router_always_returns_same_provider() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider { id_str: "stub" });
        let router = StaticRouter::new(Arc::clone(&provider));

        // 无论 task_kind 如何，始终返回同一 provider
        let picked = router.pick(Some("plan"));
        assert_eq!(picked.id(), "stub");
        let picked = router.pick(Some("code"));
        assert_eq!(picked.id(), "stub");
        let picked = router.pick(None);
        assert_eq!(picked.id(), "stub");
    }

    #[test]
    fn static_router_trait_object() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider { id_str: "stub" });
        let router: Arc<dyn Router> = Arc::new(StaticRouter::new(provider));

        // 通过 trait object 调用
        let picked = router.pick(Some("summary"));
        assert_eq!(picked.id(), "stub");
    }
}
