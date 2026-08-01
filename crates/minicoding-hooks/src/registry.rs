//! `HookRegistryImpl`：实现 `core::hooks::HookRegistry`。
//!
//! 按 `HookEvent` 索引 Hook，提供 `register`/`for_event`/`count`。`dispatch` 用
//! core trait 的默认实现（串行聚合 + L0 优先 + `on_hook_error`，见
//! `core::hooks::trait_def::HookRegistry::dispatch`）。
//!
//! 线程安全：`register` 用 `std::sync::Mutex<Vec<...>>` 保护（注册阶段短锁，
//! `for_event` 返回克隆的 `Vec`，`dispatch` 不持锁）。`Arc<dyn Hook>` 本身 `Send+Sync`。
//!
//! 详见 `docs/hooks.md` §5、`docs/modules.md` §5。

use minicoding_core::hooks::{Hook, HookEvent, HookRegistry};
use std::sync::{Arc, Mutex};

/// `HookRegistry` 的默认实现（线程安全）。
///
/// `Runtime` 持有 `Arc<HookRegistryImpl>`（或 `Arc<dyn HookRegistry>`）。
/// `dispatch` 由 core trait 默认实现提供，串行聚合所有匹配 Hook。
#[derive(Default)]
pub struct HookRegistryImpl {
    /// 全部已注册 Hook（按注册顺序）。`for_event` 时按 matcher 过滤。
    hooks: Mutex<Vec<Arc<dyn Hook>>>,
}

impl HookRegistryImpl {
    /// 创建空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self {
            hooks: Mutex::new(Vec::new()),
        }
    }

    /// 创建带初始 Hook 列表的注册表（便于测试与 CLI 批量注册）。
    #[must_use]
    pub fn with_hooks(hooks: Vec<Arc<dyn Hook>>) -> Self {
        Self {
            hooks: Mutex::new(hooks),
        }
    }
}

impl HookRegistry for HookRegistryImpl {
    fn register(&self, hook: Arc<dyn Hook>) {
        let mut guard = self.hooks.lock().expect("hooks mutex poisoned");
        guard.push(hook);
    }

    fn for_event(&self, event: HookEvent) -> Vec<Arc<dyn Hook>> {
        let guard = self.hooks.lock().expect("hooks mutex poisoned");
        guard
            .iter()
            .filter(|h| h.matcher().matches_event(event))
            .cloned()
            .collect()
    }

    fn count(&self) -> usize {
        self.hooks.lock().expect("hooks mutex poisoned").len()
    }
}

impl std::fmt::Debug for HookRegistryImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.hooks.lock().map_or(0, |g| g.len());
        f.debug_struct("HookRegistryImpl")
            .field("hook_count", &count)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::hooks::{
        DispatchConfig, HookDecision, HookInput, HookMatcher, HookOutput, OnHookError,
    };
    use minicoding_core::provider::BoxFuture;

    /// 测试用 Hook：固定返回指定输出。
    struct StaticHook {
        name: String,
        matcher: HookMatcher,
        output: HookOutput,
    }

    impl Hook for StaticHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn matcher(&self) -> &HookMatcher {
            &self.matcher
        }
        fn run(
            &self,
            _input: HookInput,
        ) -> BoxFuture<'_, Result<HookOutput, minicoding_core::hooks::HookError>> {
            let out = self.output.clone();
            Box::pin(async move { Ok(out) })
        }
    }

    #[tokio::test]
    async fn registry_register_and_count() {
        let reg = HookRegistryImpl::new();
        assert_eq!(reg.count(), 0);
        reg.register(Arc::new(StaticHook {
            name: "h1".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::continue_(),
        }));
        reg.register(Arc::new(StaticHook {
            name: "h2".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PostToolUse]),
            output: HookOutput::continue_(),
        }));
        assert_eq!(reg.count(), 2);
    }

    #[tokio::test]
    async fn registry_for_event_filters_by_matcher() {
        let reg = HookRegistryImpl::new();
        reg.register(Arc::new(StaticHook {
            name: "pre-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::continue_(),
        }));
        reg.register(Arc::new(StaticHook {
            name: "post-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PostToolUse]),
            output: HookOutput::continue_(),
        }));
        // PreToolUse 应只返回订阅了 PreToolUse 的 hook
        let pre_hooks = reg.for_event(HookEvent::PreToolUse);
        assert_eq!(pre_hooks.len(), 1);
        assert_eq!(pre_hooks[0].name(), "pre-hook");
        // PostToolUse 同理
        let post_hooks = reg.for_event(HookEvent::PostToolUse);
        assert_eq!(post_hooks.len(), 1);
        assert_eq!(post_hooks[0].name(), "post-hook");
        // SessionStart 无 hook 订阅
        assert!(reg.for_event(HookEvent::SessionStart).is_empty());
    }

    #[tokio::test]
    async fn registry_for_event_with_tool_filters_by_glob() {
        let reg = HookRegistryImpl::new();
        reg.register(Arc::new(StaticHook {
            name: "fs-hook".to_string(),
            matcher: HookMatcher::for_tools(vec![HookEvent::PreToolUse], vec!["fs.*".to_string()]),
            output: HookOutput::continue_(),
        }));
        reg.register(Arc::new(StaticHook {
            name: "shell-hook".to_string(),
            matcher: HookMatcher::for_tools(
                vec![HookEvent::PreToolUse],
                vec!["shell.run".to_string()],
            ),
            output: HookOutput::continue_(),
        }));
        // fs.write 命中 fs-hook，不命中 shell-hook
        let hooks = reg.for_event_with_tool(HookEvent::PreToolUse, Some("fs.write"));
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].name(), "fs-hook");
        // shell.run 命中 shell-hook
        let hooks = reg.for_event_with_tool(HookEvent::PreToolUse, Some("shell.run"));
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].name(), "shell-hook");
    }

    #[tokio::test]
    async fn registry_dispatch_uses_core_default_aggregation() {
        // 验证 HookRegistryImpl 复用 core 的 dispatch 默认实现
        let reg = HookRegistryImpl::new();
        reg.register(Arc::new(StaticHook {
            name: "auto-approve".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("auto"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn registry_dispatch_on_error_deny() {
        let reg = HookRegistryImpl::new();
        // 第一个 hook 超时（用极短 timeout 模拟），on_error=Deny
        reg.register(Arc::new(StaticHook {
            name: "slow-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("slow"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let config = DispatchConfig {
            on_error: OnHookError::Continue,
            timeout: std::time::Duration::from_secs(1),
            builtin_deny: None,
        };
        let result = reg.dispatch(input, config).await;
        // hook 1s 内能完成，应返回 Allow
        assert_eq!(result.decision, HookDecision::Allow);
    }
}
