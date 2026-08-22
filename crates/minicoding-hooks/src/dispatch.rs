//! Hook 分发辅助（A1 自 core trait 默认实现下沉，AGENTS.md §3.3 落位修正）。
//!
//! 分发主算法位于 `registry.rs` 的 `HookRegistryImpl::dispatch`；本文件存放其
//! 辅助件：`HookErrorAction` 错误处置映射、单次执行超时包装（`run_hook_once`）、
//! 决策合并（C-21：builtin Deny 时 Hook Allow 被忽略）。

use minicoding_core::hooks::{
    DispatchConfig, DispatchResult, Hook, HookDecision, HookError, HookInput, HookOutput,
    OnHookError,
};

/// 单次 Hook 执行的错误处置动作（`run_hook_once` 失败时返回）。
pub(super) enum HookErrorAction {
    /// `on_error=Continue`：收集错误继续下个 Hook。
    Continue(HookError),
    /// `on_error=Deny`：阻断当前操作。
    Deny(String, HookError),
    /// `on_error=Fail`：致命错误中止 turn。
    Fatal(HookError),
}

impl HookErrorAction {
    /// 按策略把 Hook 错误转为处置动作。
    pub(super) fn from_error(e: HookError, name: &str, config: &DispatchConfig) -> Self {
        match config.on_error {
            OnHookError::Continue => Self::Continue(e),
            OnHookError::Deny => Self::Deny(format!("hook `{name}` error: {e}"), e),
            OnHookError::Fail => Self::Fatal(e),
        }
    }
}

/// 运行单个 Hook（含超时），返回输出或错误处置动作。
///
/// 超时与 Hook 返回 `Err` 均按 `config.on_error` 映射为 `HookErrorAction`。
pub(super) async fn run_hook_once(
    hook: &dyn Hook,
    input: &HookInput,
    config: &DispatchConfig,
) -> Result<HookOutput, HookErrorAction> {
    let hook_name = hook.name().to_string();
    let fut = hook.run(input.clone());
    match tokio::time::timeout(config.timeout, fut).await {
        Ok(Ok(o)) => Ok(o),
        Ok(Err(e)) => Err(HookErrorAction::from_error(e, &hook_name, config)),
        Err(_) => {
            let timeout_sec = u32::try_from(config.timeout.as_secs()).unwrap_or(u32::MAX);
            let e = HookError::Timeout {
                name: hook_name.clone(),
                timeout_sec,
            };
            Err(HookErrorAction::from_error(e, &hook_name, config))
        }
    }
}

/// 合并单个 Hook 的决策到聚合结果（`HookRegistryImpl::dispatch` 内部用）。
///
/// 规则见 `HookRegistry::dispatch` 文档。C-21：内置黑名单 Deny 时，
/// Hook 的 Allow 被忽略。
pub(super) fn merge_decision(
    result: &mut DispatchResult,
    incoming: HookDecision,
    reason: Option<String>,
    config: &DispatchConfig,
) {
    match incoming {
        HookDecision::Deny => {
            result.decision = HookDecision::Deny;
            result.reason = reason.or_else(|| result.reason.take());
        }
        HookDecision::Allow => {
            // C-21：内置黑名单 Deny 时，Hook 的 Allow 被忽略
            if config.builtin_deny.is_some() {
                tracing::debug!("hook Allow ignored due to builtin blacklist Deny (C-21)");
                return;
            }
            // Allow 把 Ask/Continue 升级为 Allow；不降级已有 Deny
            if result.decision != HookDecision::Deny {
                result.decision = HookDecision::Allow;
                result.reason = reason;
            }
        }
        HookDecision::Ask => {
            // Ask 把 Continue 升级为 Ask；不降级 Allow/Deny
            if result.decision == HookDecision::Continue {
                result.decision = HookDecision::Ask;
                result.reason = reason;
            }
        }
        HookDecision::Continue => {
            // 不干预
        }
    }
}
