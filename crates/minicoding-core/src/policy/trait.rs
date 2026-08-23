//! `PermissionPolicy` / `PermissionPrompter` trait（见 `api.md` §3.6）。
//!
//! 决策（`PermissionPolicy`）与交互（`PermissionPrompter`）分离，解决 broadcast
//! 事件总线无法承载点对点回复的架构缺陷（见 `design.md` §9.1）。
//!
//! M1 简化：不强制使用权限（单轮只读工具）。M2 完整接入。
//! M5 接入：`PermissionMode`（Plan/AcceptEdits/Default/Auto/BypassPermissions，
//! 见 `design.md` §16.2）+ `PlanModeController`（`plan.exit` 工具改写 Runtime 状态）。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容。

use crate::model::SideEffect;
use crate::model::{PolicyError, SessionId};
use crate::provider::BoxFuture;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// 策略返回的中间判定（未交互）。
#[derive(Debug, Clone)]
pub enum Verdict {
    Allow,
    Deny(String),
    Ask(PermissionPrompt),
}

/// 交互后的最终决策（不再含 Ask）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny(String),
}

/// 权限模式（`design.md` §16.2）。
///
/// 与 `ApprovalMode`（§9.5）正交：`Plan` 是"工具能力面"约束（禁写），
/// `ApprovalMode` 是"何时问用户"约束。`Plan` 模式下 `ApprovalMode` 通常配
/// `OnRequest`，写操作根本进不到"问不问"那一步就被硬门拦了。
///
/// - `Default`：§9.3 默认矩阵（写 `Ask`）；
/// - `AcceptEdits`：文件写入自动 `Allow`，shell 仍 `Ask`（高频编辑场景）；
/// - `Plan`：只读强制（硬门 + 软引导，见 `design.md` §16.1）；
/// - `Auto`：分类器自动批准（含降级保护，阶段 6+，当前未启用）；
/// - `BypassPermissions`：全放行（仅隔离容器内，对齐 CC `bypassPermissions`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Default,
    AcceptEdits,
    Plan,
    Auto,
    BypassPermissions,
}

/// `plan.exit` 预批准的命令（执行期跳过 prompter，见 `design.md` §16.4）。
///
/// `tool` 与 `prompt` 同时匹配时直接 `Allow`。`prompt` 为子串匹配（如
/// `"cargo build"` 匹配 `"cargo build --release"`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct PreApprovedPrompt {
    /// 工具名（如 `"shell.run"`）。
    pub tool: String,
    /// 命令前缀（如 `"cargo build"`、`"git add"`）。
    pub prompt: String,
}

/// Plan 模式状态快照（`PlanModeController::snapshot` 返回）。
#[derive(Debug, Clone, Default)]
pub struct PlanModeSnapshot {
    /// 当前权限模式。
    pub mode: PermissionMode,
    /// `plan.exit` 缓存的预批准清单（执行期命中即 `Allow`）。
    pub allowed_prompts: Vec<PreApprovedPrompt>,
}

/// 权限上下文。
#[derive(Debug, Clone)]
pub struct PermissionContext {
    pub session: SessionId,
    pub workdir: Utf8PathBuf,
    pub side_effect: SideEffect,
    pub turn: u32,
    /// 近期决策历史（**reserved，当前恒为空**）。
    ///
    /// 设计意图：供策略参考最近 N 条真实决策（如 allowlist 启发式）。运行期
    /// 消息不回写 `session.messages`（storage 为事实源），真实决策需从
    /// `AuditSink` 回读——接入前 Runtime 填充空 `Vec`，**不得**伪造值
    /// （2026-08-23 审查 §4-P1：此前无条件填 `Allow` 会误导基于历史的策略）。
    pub history: Vec<Decision>,
    /// 当前权限模式（`Plan` 模式触发硬门，见 `design.md` §16.1）。
    pub permission_mode: PermissionMode,
    /// 预批准清单（命中即 `Allow`，跳过 `Ask`）。
    pub allowed_prompts: Vec<PreApprovedPrompt>,
}

/// 权限请求提示（点对点交互）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
pub struct PermissionPrompt {
    pub id: String,
    pub tool: String,
    pub summary: String,
    pub risk: Risk,
    pub options: Vec<PromptOption>,
}

/// 风险等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}

/// 交互选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(rename_all = "snake_case")]
pub enum PromptOption {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
}

/// TUI 权限询问的点对点消息（T-M7-3）。
///
/// 由 `TuiPrompter`（`minicoding-policy`）通过 mpsc channel 发往 TUI 主循环，
/// UI 渲染弹窗后通过 `reply` 回传 [`Decision`]，`TuiPrompter::prompt` 的 future
/// 在 await `reply` 时挂起，工具调用阻塞但 Runtime 调度器仍可推进其他 task。
///
/// 定义在 `minicoding-core` 而非 `minicoding-tui`，避免 `minicoding-policy` 反向
/// 依赖 `minicoding-tui`（依赖方向：tui → policy → core，见 AGENTS.md §3.2）。
#[derive(Debug)]
pub struct TuiPermissionRequest {
    /// 权限询问详情（工具名/摘要/风险/选项）。
    pub prompt: PermissionPrompt,
    /// UI 回传决策的 oneshot 通道。
    pub reply: oneshot::Sender<Decision>,
}

/// 纯决策 trait（无交互、无 IO，`dyn` 兼容）。
pub trait PermissionPolicy: Send + Sync {
    fn check(
        &self,
        tool: &str,
        input: &serde_json::Value,
        ctx: &PermissionContext,
    ) -> BoxFuture<'_, Result<Verdict, PolicyError>>;
}

/// 点对点交互器（非广播，`dyn` 兼容）。由 frontend 注入实现。
pub trait PermissionPrompter: Send + Sync {
    fn prompt(&self, req: PermissionPrompt) -> BoxFuture<'_, Decision>;
}

/// Plan 模式控制器（`plan.exit` 工具改写 Runtime 状态用，`dyn` 兼容）。
///
/// `plan.exit` 是 `SideEffect::None` 工具（走只读桶并行调度，不经
/// `execute_side_effect_call`），但它需要切换 `PermissionMode` 与缓存
/// `allowed_prompts`。引入该 trait 让工具持有 `Arc<dyn PlanModeController>`
/// 反向调用 Runtime，避免 core 依赖 tools。
///
/// Runtime 实现此 trait，工具通过它读写会话级 Plan 状态（见 `design.md` §16.4）。
pub trait PlanModeController: Send + Sync {
    /// 快照当前 Plan 模式状态（mode + `allowed_prompts`）。
    fn snapshot(&self) -> BoxFuture<'_, PlanModeSnapshot>;

    /// 退出 Plan 模式：切换 `mode` 为 `target_mode`，缓存 `allowed_prompts`。
    ///
    /// 仅当当前 `mode == Plan` 时可调用；其它模式下返回
    /// `ToolError::InvalidStateTransition`（C-25：`plan.exit` 仅 Plan 模式可调）。
    ///
    /// # Errors
    /// 当前非 Plan 模式时返回 `ToolError::InvalidStateTransition`。
    fn exit_plan(
        &self,
        allowed_prompts: Vec<PreApprovedPrompt>,
        target_mode: PermissionMode,
    ) -> BoxFuture<'_, Result<(), PolicyError>>;

    /// 直接切换权限模式（CLI `/plan`、`--plan` 用，非工具调用通道）。
    ///
    /// 与 `exit_plan` 区别：不校验当前是否 Plan 模式，供 CLI 显式切换。
    fn set_mode(&self, mode: PermissionMode) -> BoxFuture<'_, ()>;
}

/// 无操作策略（兜底，未注入 policy 时使用，类似 `sandbox::NoopDriver`）。
///
/// `check` 恒返回 `Verdict::Allow`——仅用于测试或未启用权限 feature 的场景，
/// 真实决策应由 `minicoding-policy::BuiltinPolicy` 提供。
pub struct NoopPolicy;

impl PermissionPolicy for NoopPolicy {
    fn check(
        &self,
        _tool: &str,
        _input: &serde_json::Value,
        _ctx: &PermissionContext,
    ) -> BoxFuture<'_, Result<Verdict, PolicyError>> {
        Box::pin(async move { Ok(Verdict::Allow) })
    }
}

/// 无操作交互器（兜底，未注入 prompter 时使用）。
///
/// `prompt` 恒返回 `Decision::Allow`——仅用于测试场景，真实交互应由
/// `minicoding-policy::InteractivePrompter`/`NonInteractivePrompter` 提供。
pub struct NoopPrompter;

impl PermissionPrompter for NoopPrompter {
    fn prompt(&self, _req: PermissionPrompt) -> BoxFuture<'_, Decision> {
        Box::pin(async move { Decision::Allow })
    }
}

#[cfg(test)]
mod tests {
    //! trait 默认实现与 noop 兜底测试（覆盖率补全）。

    use super::*;
    use crate::model::SideEffect;

    #[test]
    fn permission_mode_default_is_default() {
        assert_eq!(PermissionMode::default(), PermissionMode::Default);
    }

    #[test]
    fn plan_mode_snapshot_default_is_default_with_empty_prompts() {
        let snap = PlanModeSnapshot::default();
        assert_eq!(snap.mode, PermissionMode::Default);
        assert!(
            snap.allowed_prompts.is_empty(),
            "expected empty: snap.allowed_prompts"
        );
    }

    #[tokio::test]
    async fn noop_policy_always_allows() {
        let policy = NoopPolicy;
        let ctx = PermissionContext {
            session: "test".to_string(),
            workdir: camino::Utf8PathBuf::from("/tmp"),
            side_effect: SideEffect::None,
            turn: 0,
            history: Vec::new(),
            permission_mode: PermissionMode::Default,
            allowed_prompts: Vec::new(),
        };
        let verdict = policy
            .check("any_tool", &serde_json::Value::Null, &ctx)
            .await
            .expect("noop policy never errors");
        assert!(matches!(verdict, Verdict::Allow));
    }

    #[tokio::test]
    async fn noop_prompter_always_allows() {
        let prompter = NoopPrompter;
        let req = PermissionPrompt {
            id: "p1".to_string(),
            tool: "shell.run".to_string(),
            summary: "echo hi".to_string(),
            risk: Risk::Low,
            options: vec![PromptOption::AllowOnce, PromptOption::DenyOnce],
        };
        let decision = prompter.prompt(req).await;
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn decision_serde_roundtrip() {
        let allow = Decision::Allow;
        let json = serde_json::to_string(&allow).expect("serialize Allow");
        assert_eq!(json, "\"allow\"");
        let deny = Decision::Deny("reason".to_string());
        let json = serde_json::to_string(&deny).expect("serialize Deny");
        assert!(json.contains("deny"));
        assert!(json.contains("reason"));
    }

    #[test]
    fn permission_mode_serde_snake_case() {
        let json = serde_json::to_string(&PermissionMode::AcceptEdits).expect("serialize");
        assert_eq!(json, "\"accept_edits\"");
        let json = serde_json::to_string(&PermissionMode::BypassPermissions).expect("serialize");
        assert_eq!(json, "\"bypass_permissions\"");
        let json = serde_json::to_string(&PermissionMode::Plan).expect("serialize");
        assert_eq!(json, "\"plan\"");
    }

    #[test]
    fn pre_approved_prompt_serde_roundtrip() {
        let p = PreApprovedPrompt {
            tool: "shell.run".to_string(),
            prompt: "cargo build".to_string(),
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let decoded: PreApprovedPrompt = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.tool, p.tool);
        assert_eq!(decoded.prompt, p.prompt);
    }

    #[test]
    fn risk_serde_snake_case() {
        assert_eq!(serde_json::to_string(&Risk::Low).unwrap(), "\"low\"");
        assert_eq!(serde_json::to_string(&Risk::Medium).unwrap(), "\"medium\"");
        assert_eq!(serde_json::to_string(&Risk::High).unwrap(), "\"high\"");
    }

    #[test]
    fn prompt_option_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&PromptOption::AllowOnce).unwrap(),
            "\"allow_once\""
        );
        assert_eq!(
            serde_json::to_string(&PromptOption::AllowAlways).unwrap(),
            "\"allow_always\""
        );
        assert_eq!(
            serde_json::to_string(&PromptOption::DenyOnce).unwrap(),
            "\"deny_once\""
        );
        assert_eq!(
            serde_json::to_string(&PromptOption::DenyAlways).unwrap(),
            "\"deny_always\""
        );
    }

    #[test]
    fn verdict_variants_construct() {
        // Verdict 未派生 PartialEq，用 matches! 校验。
        let allow = Verdict::Allow;
        assert!(matches!(allow, Verdict::Allow));
        let deny = Verdict::Deny("nope".to_string());
        assert!(matches!(deny, Verdict::Deny(_)));
        let ask = Verdict::Ask(PermissionPrompt {
            id: "p1".to_string(),
            tool: "shell.run".to_string(),
            summary: "echo hi".to_string(),
            risk: Risk::Medium,
            options: vec![PromptOption::AllowOnce],
        });
        assert!(matches!(ask, Verdict::Ask(_)));
    }
}
