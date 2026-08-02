//! `--replay` 模式策略：包装内置策略，强制拒绝所有副作用工具（C-06）。
//!
//! 回放模式下，所有 `SideEffect != None` 的工具调用被硬 `Deny`，不论内层策略
//! 如何判定。只读工具（`SideEffect::None`）仍走内层策略（恒 `Allow`）。
//!
//! `--allow-side-effects` 时不应使用此策略——直接用 `BuiltinPolicy`，每条仍走
//! 权限流程（见 `getting-started.md` --replay 段、`security.md` §13.4）。

use minicoding_core::model::{PolicyError, SideEffect};
use minicoding_core::policy::{PermissionContext, PermissionPolicy, Verdict};
use minicoding_core::provider::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

/// 回放模式策略：强制拒绝所有副作用工具（C-06）。
///
/// 包装内层策略，对 `SideEffect::None` 透传内层判定，对其余副作用类别
/// 直接返回 `Deny`。`Deny` 不走 `Prompter` 交互（无需用户确认即可拒绝）。
pub struct ReplayPolicy {
    inner: Arc<dyn PermissionPolicy>,
}

impl ReplayPolicy {
    /// 创建回放策略，包装指定内层策略。
    #[must_use]
    pub fn new(inner: Arc<dyn PermissionPolicy>) -> Self {
        Self { inner }
    }
}

impl std::fmt::Debug for ReplayPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayPolicy").finish_non_exhaustive()
    }
}

impl PermissionPolicy for ReplayPolicy {
    fn check(
        &self,
        tool: &str,
        input: &Value,
        ctx: &PermissionContext,
    ) -> BoxFuture<'_, Result<Verdict, PolicyError>> {
        // C-06：回放模式下所有副作用工具硬 Deny，不透传内层策略。
        if ctx.side_effect != SideEffect::None {
            let verdict = Verdict::Deny(format!(
                "replay mode: side-effect tool '{tool}' disabled (C-06)"
            ));
            return Box::pin(async move { Ok(verdict) });
        }
        // 只读工具透传内层策略（BuiltinPolicy 对 None 恒 Allow）。
        self.inner.check(tool, input, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BuiltinPolicy;
    use camino::Utf8PathBuf;
    use minicoding_core::model::SideEffect;
    use minicoding_core::policy::{PermissionContext, PermissionMode};

    fn ctx(side_effect: SideEffect) -> PermissionContext {
        PermissionContext {
            session: "test".to_string(),
            workdir: Utf8PathBuf::from("/tmp/proj"),
            side_effect,
            turn: 0,
            history: Vec::new(),
            permission_mode: PermissionMode::Default,
            allowed_prompts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn replay_denies_file_write() {
        let policy = ReplayPolicy::new(Arc::new(BuiltinPolicy::new()));
        let input = serde_json::json!({"path": "/tmp/proj/test.txt", "content": "hi"});
        let verdict = policy
            .check("fs.write", &input, &ctx(SideEffect::FileWrite))
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Deny(_)));
    }

    #[tokio::test]
    async fn replay_denies_command() {
        let policy = ReplayPolicy::new(Arc::new(BuiltinPolicy::new()));
        let input = serde_json::json!({"command": "ls"});
        let verdict = policy
            .check("shell.run", &input, &ctx(SideEffect::Command))
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Deny(_)));
    }

    #[tokio::test]
    async fn replay_allows_readonly() {
        let policy = ReplayPolicy::new(Arc::new(BuiltinPolicy::new()));
        let input = serde_json::json!({"path": "/tmp/proj/test.txt"});
        let verdict = policy
            .check("fs.read", &input, &ctx(SideEffect::None))
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Allow));
    }

    /// C-06：`SideEffect::Network`（`web.fetch`/`web.search`）在回放模式下同样被拒。
    #[tokio::test]
    async fn replay_denies_network() {
        let policy = ReplayPolicy::new(Arc::new(BuiltinPolicy::new()));
        let input = serde_json::json!({"url": "https://example.com"});
        let verdict = policy
            .check("web.fetch", &input, &ctx(SideEffect::Network))
            .await
            .unwrap();
        assert!(
            matches!(verdict, Verdict::Deny(_)),
            "Network 副作用工具在回放模式下应被拒绝"
        );
    }

    /// C-06：所有 `SideEffect != None` 的工具在回放模式下都必须被 `Deny`，
    /// 不论工具名或输入。参数化遍历全部副作用类别，覆盖未来新增类别时回归。
    #[tokio::test]
    async fn replay_denies_all_side_effect_variants() {
        let policy = ReplayPolicy::new(Arc::new(BuiltinPolicy::new()));
        let input = serde_json::json!({});
        // 遍历全部非 None 副作用类别：FileWrite / Command / Network
        for side_effect in [
            SideEffect::FileWrite,
            SideEffect::Command,
            SideEffect::Network,
        ] {
            let verdict = policy
                .check("any.tool", &input, &ctx(side_effect))
                .await
                .unwrap();
            assert!(
                matches!(verdict, Verdict::Deny(_)),
                "回放模式下 {side_effect:?} 应被 Deny，实际: {verdict:?}"
            );
        }
        // None 仍走内层策略（BuiltinPolicy 对只读工具恒 Allow）
        let verdict = policy
            .check("fs.read", &input, &ctx(SideEffect::None))
            .await
            .unwrap();
        assert!(
            matches!(verdict, Verdict::Allow),
            "回放模式下只读工具应 Allow，实际: {verdict:?}"
        );
    }
}
