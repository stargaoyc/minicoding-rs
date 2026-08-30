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

use minicoding_core::hooks::{
    DispatchConfig, DispatchResult, Hook, HookDecision, HookError, HookEvent, HookInput,
    HookRegistry,
};
use minicoding_core::metrics;
use minicoding_core::provider::BoxFuture;
use tracing::Instrument;

use super::dispatch::{HookErrorAction, merge_decision, run_hook_once};
use std::sync::{Arc, Mutex};

/// `HookRegistry` 的默认实现（线程安全）。
///
/// `Runtime` 持有 `Arc<HookRegistryImpl>`（或 `Arc<dyn HookRegistry>`）。
/// `dispatch` 由 core trait 默认实现提供，串行聚合所有匹配 Hook。
///
/// SEC-17（2026-08-28 R5 收尾）：可选注入 `AuditSink`——Hook 协议违规
/// （ExitCode/Timeout/JSON 解析失败等 `HookError`）落 `audit.log`（AGENTS.md
/// §5.5 承诺）。未注入时为 `None`（兼容既有调用方与测试，不记审计）。
#[derive(Default)]
pub struct HookRegistryImpl {
    /// 全部已注册 Hook（按注册顺序）。`for_event` 时按 matcher 过滤。
    hooks: Mutex<Vec<Arc<dyn Hook>>>,
    /// Hook 错误审计 sink（SEC-17，可选）。
    audit: Option<Arc<dyn minicoding_core::storage::AuditSink>>,
}

impl HookRegistryImpl {
    /// 创建空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self {
            hooks: Mutex::new(Vec::new()),
            audit: None,
        }
    }

    /// 创建带初始 Hook 列表的注册表（便于测试与 CLI 批量注册）。
    #[must_use]
    pub fn with_hooks(hooks: Vec<Arc<dyn Hook>>) -> Self {
        Self {
            hooks: Mutex::new(hooks),
            audit: None,
        }
    }

    /// 注入审计 sink（SEC-17）：Hook 协议违规记 `AuditKind::HookRun`。
    ///
    /// 审计记录 best-effort（失败仅 warn，不阻断 Hook 分发——审计是记录性
    /// 保障，非执行路径的一部分，与 MCP 审批审计同策略）。
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<dyn minicoding_core::storage::AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }
}

/// 分发主算法（A1 自 core 下沉；`HookRegistryImpl` 与测试 `TestRegistry` 共用，
/// 保证测试覆盖的就是生产路径）。
async fn dispatch_hooks(
    hooks: Vec<std::sync::Arc<dyn Hook>>,
    mut input: HookInput,
    config: DispatchConfig,
    audit: Option<Arc<dyn minicoding_core::storage::AuditSink>>,
) -> DispatchResult {
    let event = input.event;

    let mut result = DispatchResult {
        decision: HookDecision::Continue,
        ..DispatchResult::default()
    };

    // L0 优先（C-21）：内置黑名单已 Deny 时，预置 Deny，Hook 的 Allow 被忽略。
    if let Some(ref reason) = config.builtin_deny {
        result.decision = HookDecision::Deny;
        result.reason = Some(reason.clone());
    }

    for hook in hooks {
        let hook_name = hook.name().to_string();
        let event_str = event.as_str();
        let span = tracing::debug_span!(
            "hook.run",
            hook = %hook_name,
            event = ?event,
            otel.name = "hook.run",
        );
        // R4（SE4-13）：`span.instrument` 替代 `span.enter()`——hook 可能跨
        // await 等待子进程完成，`Entered` guard 存活跨越 await 点，多线程
        // tokio runtime 下 future 会在 worker 线程间迁移，线程局部 span 语义
        // 失真/串号（与 R3 RT-7 同型漏网实例）。
        let hook_timer = metrics::start_timer();

        match run_hook_once(hook.as_ref(), &input, &config)
            .instrument(span)
            .await
        {
            Ok(output) => {
                let result_str = if matches!(output.decision, HookDecision::Deny) {
                    "deny"
                } else {
                    "ok"
                };
                metrics::record_hook(&hook_name, event_str, result_str);
                metrics::record_elapsed("hook_duration_ms", "hook", &hook_name, hook_timer);
                merge_decision(&mut result, output.decision, output.reason, &config);
                if let Some(new_input) = output.modify_input {
                    if let Some(ref mut tool) = input.tool {
                        tool.input = new_input.clone();
                    }
                    result.modify_input = Some(new_input);
                }
                if let Some(ctx) = output.inject_context {
                    // R9 P2-9：注入上下文包裹 hook 来源边界——Hook 可能处理不可信
                    // 内容（如 PreToolUse on `web.fetch` 结果），无来源标注时模型
                    // 无法区分"用户/项目指令"与"Hook 派生数据"。包裹后模型可识别
                    // 来源并降低对派生内容的信任（`<hook name="...">` 声明非指令）。
                    result
                        .inject_contexts
                        .push(format!("<hook name=\"{hook_name}\">\n{ctx}\n</hook>"));
                }
                if let Some(msg) = output.exit_message {
                    result.exit_messages.push(msg);
                }
                if result.async_rewake.is_none()
                    && let Some(spec) = output.async_rewake
                {
                    if event.supports_async_rewake() {
                        result.async_rewake = Some(spec);
                    } else {
                        tracing::warn!(
                            hook = %hook_name,
                            ?event,
                            "async_rewake on unsupported event, ignored"
                        );
                    }
                }
            }
            Err(action) => match action {
                HookErrorAction::Continue(e) => {
                    tracing::warn!(hook = %hook_name, error = %e, "hook error, continuing");
                    metrics::record_hook(&hook_name, event_str, "err");
                    metrics::record_error("hook");
                    record_hook_audit(audit.as_deref(), &hook_name, event_str, &e).await;
                    result.errors.push((hook_name, e));
                }
                HookErrorAction::Deny(reason, e) => {
                    tracing::warn!(hook = %hook_name, error = %e, "hook error -> deny");
                    metrics::record_hook(&hook_name, event_str, "deny");
                    metrics::record_error("hook");
                    record_hook_audit(audit.as_deref(), &hook_name, event_str, &e).await;
                    result.decision = HookDecision::Deny;
                    result.reason = Some(reason);
                    result.errors.push((hook_name, e));
                    break;
                }
                HookErrorAction::Fatal(e) => {
                    tracing::error!(hook = %hook_name, error = %e, "hook error -> fail");
                    metrics::record_hook(&hook_name, event_str, "fatal");
                    metrics::record_error("hook");
                    record_hook_audit(audit.as_deref(), &hook_name, event_str, &e).await;
                    result.fatal_error = Some(e);
                    return result;
                }
            },
        }
    }

    result
}

/// SEC-17：Hook 协议违规记审计（`AuditKind::HookRun`，best-effort）。
///
/// 触发场景：Hook 子进程 ExitCode/Timeout/JSON 解析失败等 `HookError`——这些
/// 是"协议违规"信号（AGENTS.md §5.5 要求记录）。审计失败仅 warn 不阻断分发
/// （审计是记录性保障，与 MCP 审批审计同策略）。`session` 字段用 `HookInput` 的
/// session id（如可用）否则占位。
async fn record_hook_audit(
    audit: Option<&dyn minicoding_core::storage::AuditSink>,
    hook_name: &str,
    event: &str,
    e: &HookError,
) {
    let Some(audit) = audit else { return };
    let rec = minicoding_core::storage::AuditRecord {
        ts: time::OffsetDateTime::now_utc(),
        session: "hook".to_string(),
        kind: minicoding_core::storage::AuditKind::HookRun,
        tool: Some(format!("hook__{hook_name}")),
        decision: Some("error".to_string()),
        detail: format!("hook `{hook_name}` event={event} protocol error: {e}"),
    };
    if let Err(err) = audit.record(rec).await {
        tracing::warn!(hook = %hook_name, error = %err, "hook audit record failed (best-effort)");
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
    fn dispatch(&self, input: HookInput, config: DispatchConfig) -> BoxFuture<'_, DispatchResult> {
        let hooks =
            self.for_event_with_tool(input.event, input.tool.as_ref().map(|t| t.name.as_str()));
        let audit = self.audit.clone();
        Box::pin(dispatch_hooks(hooks, input, config, audit))
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

#[cfg(test)]
mod dispatch_tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use crate::dispatch::HookErrorAction;
    use minicoding_core::hooks::{
        AsyncRewakeSpec, DispatchConfig, DispatchResult, HookDecision, HookError, HookEvent,
        HookInput, HookMatcher, HookOutput, NoopHookRegistry, OnHookError,
    };
    use std::sync::Arc;

    use camino::Utf8PathBuf;
    use minicoding_core::model::ToolCall;

    /// 测试用 `HookRegistry`（原 core trait_def tests 的 TestRegistry 随算法迁入）。
    struct TestRegistry {
        hooks: std::sync::Mutex<Vec<std::sync::Arc<dyn Hook>>>,
    }
    impl TestRegistry {
        fn new() -> Self {
            Self {
                hooks: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    impl HookRegistry for TestRegistry {
        fn register(&self, hook: std::sync::Arc<dyn Hook>) {
            self.hooks.lock().unwrap().push(hook);
        }
        fn for_event(&self, event: HookEvent) -> Vec<std::sync::Arc<dyn Hook>> {
            self.hooks
                .lock()
                .unwrap()
                .iter()
                .filter(|h| h.matcher().matches_event(event))
                .cloned()
                .collect()
        }
        fn count(&self) -> usize {
            self.hooks.lock().unwrap().len()
        }
        fn dispatch(
            &self,
            input: HookInput,
            config: DispatchConfig,
        ) -> minicoding_core::provider::BoxFuture<'_, DispatchResult> {
            let hooks =
                self.for_event_with_tool(input.event, input.tool.as_ref().map(|t| t.name.as_str()));
            Box::pin(dispatch_hooks(hooks, input, config, None))
        }
    }

    struct ErrorHook {
        name: String,
        matcher: HookMatcher,
        error: HookError,
    }

    impl Hook for ErrorHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn matcher(&self) -> &HookMatcher {
            &self.matcher
        }
        fn run(&self, _input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
            let e = self.error.clone();
            Box::pin(async move { Err(e) })
        }
    }

    /// 测试用 Hook：根据 input 决策（验证 modify_input 链式传递）。
    struct ModifyInputHook {
        name: String,
        matcher: HookMatcher,
    }

    impl Hook for ModifyInputHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn matcher(&self) -> &HookMatcher {
            &self.matcher
        }
        fn run(&self, input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
            Box::pin(async move {
                // 把 input.path 改写为加了前缀的值
                let mut new_input = input.tool.unwrap().input;
                if let Some(obj) = new_input.as_object_mut()
                    && let Some(s) = obj.get("path").and_then(|v| v.as_str()).map(String::from)
                {
                    obj.insert(
                        "path".to_string(),
                        serde_json::Value::String(format!("prefix-{s}")),
                    );
                }
                Ok(HookOutput {
                    modify_input: Some(new_input),
                    ..HookOutput::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn dispatch_no_hooks_returns_continue() {
        let reg = NoopHookRegistry;
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Continue);
        assert!(
            result.inject_contexts.is_empty(),
            "expected empty: result.inject_contexts"
        );
    }

    #[tokio::test]
    async fn dispatch_allow_upgrades_continue() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "auto-approve".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("auto"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Allow);
        assert_eq!(result.reason.as_deref(), Some("auto"));
    }

    #[tokio::test]
    async fn dispatch_deny_wins_over_allow() {
        // 注册顺序：先 Allow，后 Deny → 最终 Deny（Deny 不可被 Allow 覆盖）
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "allow-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("allow"),
        }));
        reg.register(Arc::new(StaticHook {
            name: "deny-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::deny("blocked"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Deny);
        assert_eq!(result.reason.as_deref(), Some("blocked"));
    }

    #[tokio::test]
    async fn dispatch_builtin_deny_ignores_hook_allow() {
        // C-21：内置黑名单 Deny 时，Hook 的 Allow 被忽略
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "allow-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("try to override"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let config = DispatchConfig {
            builtin_deny: Some("builtin blacklist".to_string()),
            ..DispatchConfig::default()
        };
        let result = reg.dispatch(input, config).await;
        assert_eq!(result.decision, HookDecision::Deny);
        assert_eq!(result.reason.as_deref(), Some("builtin blacklist"));
    }

    #[tokio::test]
    async fn dispatch_collects_inject_contexts() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "git-status".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::SessionStart]),
            output: HookOutput::inject("git status output"),
        }));
        reg.register(Arc::new(StaticHook {
            name: "todo".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::SessionStart]),
            output: HookOutput::inject("todo list"),
        }));
        let input = HookInput::new(HookEvent::SessionStart, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.inject_contexts.len(), 2);
        // R9 P2-9：注入上下文带 hook 来源边界
        assert!(result.inject_contexts[0].contains("git status output"));
        assert!(result.inject_contexts[0].contains("hook name=\"git-status\""));
        assert!(result.inject_contexts[1].contains("todo list"));
        assert!(result.inject_contexts[1].contains("hook name=\"todo\""));
    }

    #[tokio::test]
    async fn dispatch_modify_input_chains() {
        // 两个 ModifyInputHook 串联：第一个改 path 为 prefix-xxx，第二个再改为 prefix-prefix-xxx
        let reg = TestRegistry::new();
        reg.register(Arc::new(ModifyInputHook {
            name: "modifier-1".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
        }));
        reg.register(Arc::new(ModifyInputHook {
            name: "modifier-2".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
        }));
        let mut input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        input.tool = Some(ToolCall {
            id: "c1".to_string(),
            name: "fs.write".to_string(),
            input: serde_json::json!({"path": "src/main.rs"}),
        });
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        let final_input = result.modify_input.expect("应有 modify_input");
        assert_eq!(
            final_input["path"],
            serde_json::Value::String("prefix-prefix-src/main.rs".to_string())
        );
    }

    #[tokio::test]
    async fn dispatch_on_error_continue_collects_errors() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(ErrorHook {
            name: "fail-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            error: HookError::Internal("boom".to_string()),
        }));
        reg.register(Arc::new(StaticHook {
            name: "ok-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("after error"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        // on_error=Continue：错误收集到 errors，继续执行下个 hook
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn dispatch_on_error_deny_blocks() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(ErrorHook {
            name: "fail-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            error: HookError::Internal("boom".to_string()),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let config = DispatchConfig {
            on_error: OnHookError::Deny,
            ..DispatchConfig::default()
        };
        let result = reg.dispatch(input, config).await;
        assert_eq!(result.decision, HookDecision::Deny);
        assert!(result.reason.as_deref().unwrap().contains("fail-hook"));
    }

    #[tokio::test]
    async fn dispatch_async_rewake_only_on_supported_events() {
        // PostToolUse 支持 async_rewake
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "rewake-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PostToolUse]),
            output: HookOutput {
                async_rewake: Some(AsyncRewakeSpec {
                    estimated_duration_sec: 10,
                    description: "cargo audit".to_string(),
                }),
                ..HookOutput::default()
            },
        }));
        let input = HookInput::new(HookEvent::PostToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert!(result.async_rewake.is_some());

        // PreToolUse 不支持 async_rewake → 被忽略
        let reg2 = TestRegistry::new();
        reg2.register(Arc::new(StaticHook {
            name: "rewake-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput {
                async_rewake: Some(AsyncRewakeSpec {
                    estimated_duration_sec: 10,
                    description: "should be ignored".to_string(),
                }),
                ..HookOutput::default()
            },
        }));
        let input2 = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result2 = reg2.dispatch(input2, DispatchConfig::default()).await;
        assert!(result2.async_rewake.is_none());
    }

    // ===== HookEvent 完整变体覆盖 =====

    /// 固定输出 Hook。
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
        ) -> minicoding_core::provider::BoxFuture<'_, Result<HookOutput, HookError>> {
            let out = self.output.clone();
            Box::pin(async move { Ok(out) })
        }
    }

    // ===== dispatch 决策聚合（补充分支）=====

    #[tokio::test]
    async fn dispatch_deny_does_not_downgrade_to_allow() {
        // 先 Deny，再 Allow → 最终 Deny（Allow 不能降级 Deny）
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "deny-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::deny("blocked"),
        }));
        reg.register(Arc::new(StaticHook {
            name: "allow-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("try override"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Deny);
        // 第一个 Deny 的 reason 被保留
        assert_eq!(result.reason.as_deref(), Some("blocked"));
    }

    #[tokio::test]
    async fn dispatch_ask_upgrades_continue() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "ask-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput {
                decision: HookDecision::Ask,
                reason: Some("need user input".to_string()),
                ..HookOutput::default()
            },
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Ask);
        assert_eq!(result.reason.as_deref(), Some("need user input"));
    }

    #[tokio::test]
    async fn dispatch_ask_does_not_downgrade_allow() {
        // 先 Allow，再 Ask → 最终 Allow（Ask 不能降级 Allow）
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "allow-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("approved"),
        }));
        reg.register(Arc::new(StaticHook {
            name: "ask-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput {
                decision: HookDecision::Ask,
                reason: Some("ask".to_string()),
                ..HookOutput::default()
            },
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Allow);
        assert_eq!(result.reason.as_deref(), Some("approved"));
    }

    #[tokio::test]
    async fn dispatch_allow_upgrades_ask() {
        // 先 Ask，再 Allow → 最终 Allow（Allow 升级 Ask）
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "ask-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput {
                decision: HookDecision::Ask,
                reason: Some("ask".to_string()),
                ..HookOutput::default()
            },
        }));
        reg.register(Arc::new(StaticHook {
            name: "allow-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("approved"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Allow);
        assert_eq!(result.reason.as_deref(), Some("approved"));
    }

    #[tokio::test]
    async fn dispatch_deny_without_reason_keeps_existing() {
        // 第一个 Deny 有 reason，第二个 Deny 无 reason → 保留第一个 reason
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "deny-1".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput {
                decision: HookDecision::Deny,
                reason: Some("first reason".to_string()),
                ..HookOutput::default()
            },
        }));
        reg.register(Arc::new(StaticHook {
            name: "deny-2".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput {
                decision: HookDecision::Deny,
                reason: None,
                ..HookOutput::default()
            },
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Deny);
        assert_eq!(result.reason.as_deref(), Some("first reason"));
    }

    #[tokio::test]
    async fn dispatch_deny_with_reason_replaces_existing() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "deny-1".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::deny("first reason"),
        }));
        reg.register(Arc::new(StaticHook {
            name: "deny-2".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::deny("second reason"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Deny);
        assert_eq!(result.reason.as_deref(), Some("second reason"));
    }

    #[tokio::test]
    async fn dispatch_continue_keeps_existing_decision() {
        // 第一个 Allow，第二个 Continue → 最终 Allow（Continue 不干预）
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "allow-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("approved"),
        }));
        reg.register(Arc::new(StaticHook {
            name: "noop-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::continue_(),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    // ===== dispatch on_error=Fail =====

    #[tokio::test]
    async fn dispatch_on_error_fail_returns_fatal() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(ErrorHook {
            name: "fail-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            error: HookError::Internal("boom".to_string()),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let config = DispatchConfig {
            on_error: OnHookError::Fail,
            ..DispatchConfig::default()
        };
        let result = reg.dispatch(input, config).await;
        assert!(result.fatal_error.is_some());
        let err = result.fatal_error.as_ref().expect("fatal");
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn dispatch_on_error_fail_skips_subsequent_hooks() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(ErrorHook {
            name: "fail-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            error: HookError::Internal("boom".to_string()),
        }));
        reg.register(Arc::new(StaticHook {
            name: "after-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::allow("after"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let config = DispatchConfig {
            on_error: OnHookError::Fail,
            ..DispatchConfig::default()
        };
        let result = reg.dispatch(input, config).await;
        // Fail 短路：第二个 hook 不执行
        assert!(result.fatal_error.is_some());
        assert!(result.errors.is_empty(), "expected empty: result.errors");
        assert_eq!(result.decision, HookDecision::Continue);
    }

    // ===== dispatch 超时（run_hook_once 超时分支）=====

    /// 测试用 Hook：模拟慢速 Hook（用于超时测试）。
    struct SlowHook {
        name: String,
        matcher: HookMatcher,
        delay: std::time::Duration,
        output: HookOutput,
    }

    impl Hook for SlowHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn matcher(&self) -> &HookMatcher {
            &self.matcher
        }
        fn run(&self, _input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
            let out = self.output.clone();
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok(out)
            })
        }
    }

    // F1：start_paused 虚拟时钟——SlowHook 的 200ms 与 dispatch 的 50ms 超时
    // 同一虚拟时间线即时推进，超时必然先于 hook 完成（真实时钟下为竞态）。
    #[tokio::test(start_paused = true)]
    async fn dispatch_timeout_on_error_continue() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(SlowHook {
            name: "slow-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            delay: std::time::Duration::from_millis(200),
            output: HookOutput::allow("slow ok"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let config = DispatchConfig {
            timeout: std::time::Duration::from_millis(50),
            ..DispatchConfig::default()
        };
        let result = reg.dispatch(input, config).await;
        // 超时按 Continue 收集错误
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.decision, HookDecision::Continue);
        let (name, err) = &result.errors[0];
        assert_eq!(name, "slow-hook");
        assert!(matches!(err, HookError::Timeout { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn dispatch_timeout_on_error_deny() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(SlowHook {
            name: "slow-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            delay: std::time::Duration::from_millis(200),
            output: HookOutput::allow("slow ok"),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let config = DispatchConfig {
            timeout: std::time::Duration::from_millis(50),
            on_error: OnHookError::Deny,
            ..DispatchConfig::default()
        };
        let result = reg.dispatch(input, config).await;
        assert_eq!(result.decision, HookDecision::Deny);
        assert_eq!(result.errors.len(), 1);
    }

    // ===== dispatch exit_messages / async_rewake / modify_input 边界 =====

    #[tokio::test]
    async fn dispatch_exit_messages_collected() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "exit-1".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::Stop]),
            output: HookOutput {
                exit_message: Some("exiting 1".to_string()),
                ..HookOutput::default()
            },
        }));
        reg.register(Arc::new(StaticHook {
            name: "exit-2".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::Stop]),
            output: HookOutput {
                exit_message: Some("exiting 2".to_string()),
                ..HookOutput::default()
            },
        }));
        let input = HookInput::new(HookEvent::Stop, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.exit_messages.len(), 2);
        assert_eq!(result.exit_messages[0], "exiting 1");
        assert_eq!(result.exit_messages[1], "exiting 2");
    }

    #[tokio::test]
    async fn dispatch_async_rewake_first_wins() {
        // 两个 Hook 都产生 async_rewake → 第一个胜出
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "rewake-1".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PostToolUse]),
            output: HookOutput {
                async_rewake: Some(AsyncRewakeSpec {
                    estimated_duration_sec: 10,
                    description: "first".to_string(),
                }),
                ..HookOutput::default()
            },
        }));
        reg.register(Arc::new(StaticHook {
            name: "rewake-2".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PostToolUse]),
            output: HookOutput {
                async_rewake: Some(AsyncRewakeSpec {
                    estimated_duration_sec: 20,
                    description: "second".to_string(),
                }),
                ..HookOutput::default()
            },
        }));
        let input = HookInput::new(HookEvent::PostToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        let spec = result.async_rewake.expect("async_rewake");
        assert_eq!(spec.description, "first");
        assert_eq!(spec.estimated_duration_sec, 10);
    }

    #[tokio::test]
    async fn dispatch_modify_input_without_tool_in_input() {
        // input.tool = None：modify_input 仍写入 result，但不更新 input.tool
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "modify-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput {
                modify_input: Some(serde_json::json!({"path": "alt.rs"})),
                ..HookOutput::default()
            },
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert!(result.modify_input.is_some());
        assert_eq!(result.modify_input.expect("input")["path"], "alt.rs");
    }

    #[tokio::test]
    async fn dispatch_filters_by_tool_name_via_matcher() {
        let reg = TestRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "fs-hook".to_string(),
            matcher: HookMatcher::for_tools(
                vec![HookEvent::PreToolUse],
                vec!["fs.write".to_string()],
            ),
            output: HookOutput::allow("fs ok"),
        }));
        // input 带 fs.write → 匹配
        let mut input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        input.tool = Some(ToolCall {
            id: "c1".to_string(),
            name: "fs.write".to_string(),
            input: serde_json::json!({}),
        });
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.decision, HookDecision::Allow);

        // input 带 shell.run → 不匹配
        let mut input2 = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        input2.tool = Some(ToolCall {
            id: "c2".to_string(),
            name: "shell.run".to_string(),
            input: serde_json::json!({}),
        });
        let result2 = reg.dispatch(input2, DispatchConfig::default()).await;
        assert_eq!(result2.decision, HookDecision::Continue);
    }

    // ===== HookErrorAction::from_error =====

    #[test]
    fn hook_error_action_from_error_continue() {
        let config = DispatchConfig::default();
        let e = HookError::Internal("x".to_string());
        let action = HookErrorAction::from_error(e, "h", &config);
        assert!(matches!(action, HookErrorAction::Continue(_)));
    }

    #[test]
    fn hook_error_action_from_error_deny() {
        let config = DispatchConfig {
            on_error: OnHookError::Deny,
            ..DispatchConfig::default()
        };
        let e = HookError::Internal("x".to_string());
        let action = HookErrorAction::from_error(e, "h", &config);
        let HookErrorAction::Deny(reason, _) = action else {
            panic!("expected Deny action");
        };
        assert!(reason.contains("h"));
    }

    #[test]
    fn hook_error_action_from_error_fail() {
        let config = DispatchConfig {
            on_error: OnHookError::Fail,
            ..DispatchConfig::default()
        };
        let e = HookError::Internal("x".to_string());
        let action = HookErrorAction::from_error(e, "h", &config);
        assert!(matches!(action, HookErrorAction::Fatal(_)));
    }

    /// 测试用审计 sink（SEC-17）：收集 record 供断言。
    #[derive(Clone, Default)]
    struct RecordingAudit(
        std::sync::Arc<std::sync::Mutex<Vec<minicoding_core::storage::AuditRecord>>>,
    );

    impl minicoding_core::storage::AuditSink for RecordingAudit {
        fn record(
            &self,
            rec: minicoding_core::storage::AuditRecord,
        ) -> BoxFuture<'_, Result<(), minicoding_core::model::StorageError>> {
            let inner = self.0.clone();
            Box::pin(async move {
                inner.lock().expect("audit lock").push(rec);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn hook_protocol_error_records_hook_run_audit() {
        // SEC-17（2026-08-28 R5 收尾）：注入 AuditSink 后 Hook 协议违规
        // 必须落 AuditKind::HookRun（AGENTS.md §5.5 承诺）；未注入时不记。
        let audit = RecordingAudit::default();
        let reg = HookRegistryImpl::new().with_audit(Arc::new(audit.clone()));
        reg.register(Arc::new(ErrorHook {
            name: "audit-fail".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            error: HookError::Internal("boom".to_string()),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.errors.len(), 1, "错误应收敛");

        let records = audit.0.lock().expect("audit lock");
        assert_eq!(records.len(), 1, "应恰有一条 HookRun 审计记录");
        assert_eq!(
            records[0].kind,
            minicoding_core::storage::AuditKind::HookRun
        );
        assert!(
            records[0].detail.contains("audit-fail"),
            "detail 应含 hook 名: {}",
            records[0].detail
        );
    }

    #[tokio::test]
    async fn hook_protocol_error_without_audit_is_noop() {
        // 未注入 audit（默认/测试注册表）不记审计——兼容既有调用方。
        let reg = HookRegistryImpl::new();
        reg.register(Arc::new(ErrorHook {
            name: "no-audit".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            error: HookError::Internal("boom".to_string()),
        }));
        let input = HookInput::new(HookEvent::PreToolUse, "s", 1, Utf8PathBuf::from("/tmp"));
        let result = reg.dispatch(input, DispatchConfig::default()).await;
        assert_eq!(result.errors.len(), 1);
    }
}
