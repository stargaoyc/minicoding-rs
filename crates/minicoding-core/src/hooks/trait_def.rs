//! `Hook` trait + `HookRegistry` trait + 事件/输入输出 DTO。

use crate::model::{SideEffect, ToolCall};
use crate::provider::BoxFuture;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Hook 事件类型（10 类，见 `hooks.md` §2）。
///
/// 7 类纯同步 + 3 类同步/异步可选（`PostToolUse`/`PostToolUseFailure`/`Stop`）。
/// `asyncRewake` 不是第 11 类事件，而是这 3 类事件的子模式（见 `hooks.md` §11）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HookEvent {
    /// 会话开始/resume 前。可注入上下文（git status/TODO/环境信息）。
    SessionStart,
    /// 用户提交后、LLM 调用前。可阻断（拒绝提交）/注入上下文。
    UserPromptSubmit,
    /// `policy.check` 后、工具执行前。可阻断/改写 input/注入上下文。
    PreToolUse,
    /// 工具执行成功后、结果回灌前。可改写 result/跑 formatter（同步/异步可选）。
    PostToolUse,
    /// 工具执行失败后、错误回灌前。可改写 error/诊断（同步/异步可选）。
    PostToolUseFailure,
    /// 上下文压缩管道启动前。可注入保留指令。
    PreCompact,
    /// 上下文压缩完成后。可补充注入丢失的关键上下文。
    PostCompact,
    /// 主 Agent 一轮结束。可要求继续/跑测试/生成摘要（同步/异步可选）。
    Stop,
    /// 子 Agent 完成。可校验子任务产出。
    SubagentStop,
    /// `Verdict::Ask` 即将弹窗前。可直接给 `Decision` 跳过 Prompter。
    PermissionRequest,
}

impl HookEvent {
    /// 该事件是否支持 `async_rewake`（仅 3 类"事后"事件，见 `hooks.md` §2）。
    #[must_use]
    pub fn supports_async_rewake(self) -> bool {
        matches!(
            self,
            Self::PostToolUse | Self::PostToolUseFailure | Self::Stop
        )
    }

    /// 该事件是否可阻断主流程（见 `hooks.md` §2 "可否阻断"列）。
    #[must_use]
    pub fn can_block(self) -> bool {
        matches!(
            self,
            Self::UserPromptSubmit | Self::PreToolUse | Self::Stop | Self::PermissionRequest
        )
    }

    /// 该事件是否可改写载荷（input/result/error，见 `hooks.md` §2 "可否改写"列）。
    #[must_use]
    pub fn can_modify(self) -> bool {
        matches!(
            self,
            Self::PreToolUse | Self::PostToolUse | Self::PostToolUseFailure
        )
    }

    /// 该事件是否为"工具相关事件"（`tools` matcher 过滤仅对这 4 类有效）。
    ///
    /// 工具相关：`PreToolUse`/`PostToolUse`/`PostToolUseFailure`/`PermissionRequest`。
    /// 非工具事件（`SessionStart` 等）忽略 `tools` 过滤（见 `HookMatcher.tools` 文档）。
    #[must_use]
    pub fn is_tool_event(self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::PostToolUse
                | Self::PostToolUseFailure
                | Self::PermissionRequest
        )
    }

    /// 事件名（kebab-case，用于 Hook 命名/审计/日志，T-M5-8）。
    ///
    /// 与 `serde` 的 `PascalCase` 序列化不同，这里返回小写事件名便于在
    /// `ScriptHook` 名字中嵌入（如 `"pre_tool_use[0]"`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::PostToolUseFailure => "post_tool_use_failure",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
            Self::Stop => "stop",
            Self::SubagentStop => "subagent_stop",
            Self::PermissionRequest => "permission_request",
        }
    }
}

/// Hook 命中匹配器：订阅的事件 + 可选的工具名 glob。
///
/// `tools = None` 表示匹配所有工具（仅对 `PreToolUse`/`PostToolUse`/
/// `PostToolUseFailure`/`PermissionRequest` 有效；其他事件忽略 `tools`）。
#[derive(Debug, Clone)]
pub struct HookMatcher {
    /// 订阅的事件列表。
    pub events: Vec<HookEvent>,
    /// 工具名 glob 列表（`|` 分隔、`*` 通配），`None` = 所有工具。
    pub tools: Option<Vec<String>>,
}

impl HookMatcher {
    /// 创建匹配指定事件的 matcher，匹配所有工具。
    #[must_use]
    pub fn for_events(events: Vec<HookEvent>) -> Self {
        Self {
            events,
            tools: None,
        }
    }

    /// 创建匹配指定事件 + 指定工具的 matcher。
    #[must_use]
    pub fn for_tools(events: Vec<HookEvent>, tools: Vec<String>) -> Self {
        Self {
            events,
            tools: Some(tools),
        }
    }

    /// 判断 matcher 是否订阅了某事件。
    #[must_use]
    pub fn matches_event(&self, event: HookEvent) -> bool {
        self.events.contains(&event)
    }

    /// 判断 matcher 是否订阅了某事件 + 某工具名。
    /// 非工具相关事件（`SessionStart` 等）忽略 `tool_name`，仅看事件。
    #[must_use]
    pub fn matches(&self, event: HookEvent, tool_name: Option<&str>) -> bool {
        if !self.matches_event(event) {
            return false;
        }
        // 非工具事件忽略 tools 过滤（见 `HookMatcher.tools` 文档与 `HookEvent::is_tool_event`）
        if !event.is_tool_event() {
            return true;
        }
        match (&self.tools, tool_name) {
            // 无工具过滤（None）或无工具名时放行
            (None, _) | (Some(_), None) => true,
            (Some(patterns), Some(name)) => patterns.iter().any(|p| glob_match(p, name)),
        }
    }
}

/// 简单 glob 匹配：`*` 匹配任意字符序列，`|` 分隔多个模式。
/// 不引入 `globset` 重依赖（core 必须轻量）；复杂 glob 由实现 crate 处理。
fn glob_match(pattern: &str, name: &str) -> bool {
    // `|` 分隔：任一模式命中即可。
    pattern
        .split('|')
        .any(|p| single_glob_match(p.trim(), name))
}

/// 单个 glob 模式匹配（仅支持 `*` 通配）。
fn single_glob_match(pattern: &str, name: &str) -> bool {
    // 精确匹配快速路径。
    if !pattern.contains('*') {
        return pattern == name;
    }
    // `*` 分割后做贪心匹配。
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.is_empty() {
        return true;
    }
    // 前缀匹配。
    if !name.starts_with(parts[0]) {
        return false;
    }
    let mut rest = &name[parts[0].len()..];
    // 中间部分依次查找。
    for part in &parts[1..parts.len() - 1] {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(idx) => rest = &rest[idx + part.len()..],
            None => return false,
        }
    }
    // 后缀匹配。
    let last = parts[parts.len() - 1];
    rest.ends_with(last)
}

/// Hook 输入（Runtime → Hook）。
///
/// 字段按事件类型裁剪：`SessionStart` 不含 `tool`；`PreCompact` 含 `tokens_before`/
/// `tokens_after` 预估值（放 `extras`）；`PermissionRequest` 含 `prompt` 摘要（放 `extras`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookInput {
    /// 触发的事件类型。
    pub event: HookEvent,
    /// 当前会话 ID。
    pub session_id: String,
    /// 当前轮次序号。
    pub turn: u32,
    /// 触发该事件的工具调用（仅工具相关事件有值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolCall>,
    /// 工具副作用的类别（仅工具相关事件有值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side_effect: Option<SideEffect>,
    /// `policy.check` 的中间判定（仅 `PreToolUse`/`PermissionRequest` 有值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<VerdictSerde>,
    /// 当前工作目录。
    pub cwd: Utf8PathBuf,
    /// 事件特有字段（`tokens_before`/`tokens_after`/`prompt` 摘要等）。
    #[serde(default)]
    pub extras: serde_json::Value,
}

/// `Verdict` 的可序列化表示（`Verdict` 本身不实现 `Serialize`，因含 `PermissionPrompt`）。
///
/// Hook 收到的 `verdict` 字段仅用于信息展示与决策参考，不用于反序列化回 `Verdict`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerdictSerde {
    Allow,
    Deny { reason: String },
    Ask { tool: String, summary: String },
}

impl HookInput {
    /// 创建最小输入（非工具事件用）。
    #[must_use]
    pub fn new(
        event: HookEvent,
        session_id: impl Into<String>,
        turn: u32,
        cwd: Utf8PathBuf,
    ) -> Self {
        Self {
            event,
            session_id: session_id.into(),
            turn,
            tool: None,
            side_effect: None,
            verdict: None,
            cwd,
            extras: serde_json::Value::Null,
        }
    }
}

/// Hook 输出（Hook → Runtime）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HookOutput {
    /// 决策（`Allow`/`Deny`/`Ask`/`Continue`，仅 `PreToolUse`/`PermissionRequest`/`Stop` 有效）。
    #[serde(default = "default_decision")]
    pub decision: HookDecision,
    /// 决策原因（回灌 LLM 或写审计）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 改写后的工具入参（仅 `PreToolUse`，仍经 `sandbox_path` 校验，C-21/C-03）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modify_input: Option<serde_json::Value>,
    /// 注入上下文（`SessionStart`/`UserPromptSubmit`/`PreCompact`/`PostCompact`，
    /// Runtime 包裹 `<hook_context>` 边界后追加，声明非指令，C-05）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inject_context: Option<String>,
    /// 展示给用户的退出消息。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_message: Option<String>,
    /// 异步唤醒规格（仅 `PostToolUse`/`PostToolUseFailure`/`Stop` 有效，见 `hooks.md` §11）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub async_rewake: Option<AsyncRewakeSpec>,
}

fn default_decision() -> HookDecision {
    HookDecision::Continue
}

impl HookOutput {
    /// 创建"不干预"输出。
    #[must_use]
    pub fn continue_() -> Self {
        Self::default()
    }

    /// 创建"允许"输出。
    #[must_use]
    pub fn allow(reason: impl Into<String>) -> Self {
        Self {
            decision: HookDecision::Allow,
            reason: Some(reason.into()),
            ..Self::default()
        }
    }

    /// 创建"拒绝"输出。
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            decision: HookDecision::Deny,
            reason: Some(reason.into()),
            ..Self::default()
        }
    }

    /// 创建"注入上下文"输出。
    #[must_use]
    pub fn inject(context: impl Into<String>) -> Self {
        Self {
            inject_context: Some(context.into()),
            ..Self::default()
        }
    }
}

/// Hook 决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDecision {
    /// 批准执行（覆盖 `Ask→Allow`，不可覆盖内置黑名单 `Deny`，C-21）。
    Allow,
    /// 阻断执行。
    Deny,
    /// 仍走交互（`PreToolUse` 把决策权交回 Prompter）。
    Ask,
    /// 不干预（默认）。
    #[default]
    Continue,
}

/// 异步唤醒规格（仅 `PostToolUse`/`PostToolUseFailure`/`Stop` 有效，见 `hooks.md` §11）。
///
/// Hook 同步返回 `async_rewake = Some(spec)` 后主流程不阻塞，Hook 子进程在后台继续执行，
/// 完成后唤醒 Agent 注入结果。后台进程遵守凭证隔离（C-04）、沙箱（C-22）、路径沙箱
/// （C-03）约束（C-26）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncRewakeSpec {
    /// 后台任务预估执行时长（秒），超时为 `estimated_duration × 2`（C-32）。
    pub estimated_duration_sec: u32,
    /// 后台任务的可读描述（用于审计与日志）。
    pub description: String,
}

/// Hook 错误。
#[derive(Debug, Clone, thiserror::Error)]
pub enum HookError {
    /// Hook 执行超时。
    #[error("hook `{name}` 超时（{timeout_sec}s）")]
    Timeout { name: String, timeout_sec: u32 },
    /// Hook 子进程非零退出且非 2（deny）。
    #[error("hook `{name}` 退出码 {code}: {stderr}")]
    ExitCode {
        name: String,
        code: i32,
        stderr: String,
    },
    /// Hook 输出 JSON 解析失败。
    #[error("hook `{name}` 输出解析失败: {reason}")]
    InvalidOutput { name: String, reason: String },
    /// Hook 内部错误（Rust 实现的 Hook）。
    #[error("hook 内部错误: {0}")]
    Internal(String),
}

/// `on_hook_error` 策略（见 `hooks.md` §6）。
///
/// Hook 超时/非零退出（非 2）时的处理方式。默认 `Continue`（记 warn 不中断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnHookError {
    /// 继续主流程，记 warn 日志与审计（默认）。
    #[default]
    Continue,
    /// 视为 `Deny` 阻断当前操作。
    Deny,
    /// 视为致命错误，返回 `Err` 中止当前 turn（仅用于强约束场景）。
    Fail,
}

/// Hook 分发配置（Runtime 调用 `dispatch` 时传入）。
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    /// `on_hook_error` 策略。
    pub on_error: OnHookError,
    /// 单个 Hook 超时（默认 30s，见 `hooks.md` §6）。
    pub timeout: std::time::Duration,
    /// 内置黑名单 `Deny` 原因（C-21：内置黑名单 Deny 优先于 Hook，
    /// Hook 的 `Allow` 被忽略）。
    ///
    /// `Some` 表示策略层已判定为内置黑名单 `Deny`，Hook 无法翻案；
    /// `None` 表示无内置黑名单 Deny，Hook 可正常决策。
    pub builtin_deny: Option<String>,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            on_error: OnHookError::Continue,
            timeout: std::time::Duration::from_secs(30),
            builtin_deny: None,
        }
    }
}

/// Hook 分发结果（串行聚合所有匹配 Hook 后的最终输出）。
#[derive(Debug, Clone, Default)]
pub struct DispatchResult {
    /// 最终决策（聚合后）。
    pub decision: HookDecision,
    /// 决策原因（`Deny`/`Allow` 时有值）。
    pub reason: Option<String>,
    /// 改写后的工具入参（仅 `PreToolUse`，最后一个产生 `modify_input` 的 Hook 胜出）。
    pub modify_input: Option<serde_json::Value>,
    /// 注入上下文列表（多个 Hook 的 `inject_context` 收集后由调用方拼接，
    /// 包裹 `<hook_context>` 边界，C-05）。
    pub inject_contexts: Vec<String>,
    /// 展示给用户的退出消息列表。
    pub exit_messages: Vec<String>,
    /// 异步唤醒规格（仅 `PostToolUse`/`PostToolUseFailure`/`Stop` 有效，
    /// 第一个产生的 `async_rewake` 胜出）。
    pub async_rewake: Option<AsyncRewakeSpec>,
    /// 非致命错误列表（`on_error=Continue` 时收集，供审计）。
    pub errors: Vec<(String, HookError)>,
    /// 是否因 `on_error=Fail` 中止（调用方据此返回 `Err`）。
    pub fatal_error: Option<HookError>,
}

/// 内建/SDK 用的进程内 Hook；外部脚本走 `ScriptHook` 适配器实现本 trait。
///
/// 与 `Tool` trait 一致，用 `BoxFuture` 返回类型保证 `dyn` 兼容（`async fn in trait`
/// 的 `dyn` 兼容需 boxed future）。Runtime 持有 `Arc<dyn Hook>`。
pub trait Hook: Send + Sync {
    /// 唯一名（审计与日志用）。
    fn name(&self) -> &str;
    /// 命中哪些事件与工具（matcher）。
    fn matcher(&self) -> &HookMatcher;
    /// 处理事件。
    fn run(&self, input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>>;
}

/// Hook 注册表 trait（Runtime 持有 `Arc<dyn HookRegistry>`）。
///
/// 实现在 `minicoding-hooks`（`HookRegistryImpl`）。core 提供 `NoopHookRegistry` 兜底
/// （未启用 hooks feature 时使用）。
///
/// `dispatch` 默认实现处理串行聚合、超时、`on_hook_error`、L0 优先（C-21）——这是
/// 编排逻辑而非领域实现，故放在 core（见 `AGENTS.md` §3.4）。子进程协议解析
/// （`ScriptHook`）在 `minicoding-hooks`。
pub trait HookRegistry: Send + Sync {
    /// 注册一个 Hook。
    fn register(&self, hook: Arc<dyn Hook>);

    /// 按事件取出有序 Hook 列表（按注册顺序）。
    /// Runtime 串行执行，聚合 `modify_input`/`inject_context`。
    fn for_event(&self, event: HookEvent) -> Vec<Arc<dyn Hook>>;

    /// 按 事件+工具名 取出有序 Hook 列表（matcher 过滤）。
    fn for_event_with_tool(&self, event: HookEvent, tool_name: Option<&str>) -> Vec<Arc<dyn Hook>> {
        self.for_event(event)
            .into_iter()
            .filter(|h| h.matcher().matches(event, tool_name))
            .collect()
    }

    /// 已注册的 Hook 总数（诊断/`doctor` 用）。
    fn count(&self) -> usize;

    /// 分发事件：按注册顺序串行执行所有匹配 Hook，聚合输出。
    ///
    /// 聚合规则（见 `hooks.md` §4）：
    /// - **decision**：`Deny` 立即短路（阻断）；`Allow` 把 `Ask/Continue` 升级为 `Allow`；
    ///   `Ask` 把 `Continue` 升级为 `Ask`；`Continue` 不变。`Allow` 不可降级已有 `Deny`。
    /// - **L0 优先（C-21）**：`config.builtin_deny = Some` 时，Hook 的 `Allow` 被忽略，
    ///   最终决策恒为 `Deny`（内置黑名单不可被 Hook 覆盖）。
    /// - **`modify_input`**（`PreToolUse`）：前一个 Hook 的 `modify_input` 作为后一个
    ///   Hook 的 `input.tool.input`；最终为最后一个产生该字段的 Hook 输出。
    /// - **`inject_context`**：所有 Hook 的 `inject_context` 收集到 `inject_contexts`，
    ///   调用方拼接后包裹 `<hook_context>` 边界注入（C-05）。
    /// - **`on_hook_error`**：Hook 超时/错误时按 `config.on_error` 处理
    ///   （`Continue` 收集到 `errors` 继续下个 Hook；`Deny` 短路；`Fail` 置
    ///   `fatal_error` 中止）。
    /// - **`async_rewake`**：第一个产生的 `async_rewake` 胜出（C-32 并发上限由调用方
    ///   在 `minicoding-hooks` 的 `asyncRewake` 管理器处强制）。
    ///
    /// 默认实现适用于所有 `HookRegistry` 实现（`NoopHookRegistry` 无 Hook 时返回空结果）。
    fn dispatch(&self, input: HookInput, config: DispatchConfig) -> BoxFuture<'_, DispatchResult>;
}

/// 空实现（未启用 hooks feature 时兜底）。
#[derive(Debug, Default, Clone)]
pub struct NoopHookRegistry;

impl HookRegistry for NoopHookRegistry {
    fn register(&self, _hook: Arc<dyn Hook>) {}

    fn for_event(&self, _event: HookEvent) -> Vec<Arc<dyn Hook>> {
        Vec::new()
    }

    fn count(&self) -> usize {
        0
    }

    /// 下沉后 Noop 自行实现：无 hook 但 C-21 `builtin_deny` 预置 Deny 仍生效。
    fn dispatch(&self, _input: HookInput, config: DispatchConfig) -> BoxFuture<'_, DispatchResult> {
        let mut result = DispatchResult::default();
        if let Some(reason) = config.builtin_deny {
            result.decision = HookDecision::Deny;
            result.reason = Some(reason);
        }
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn event_supports_async_rewake() {
        assert!(HookEvent::PostToolUse.supports_async_rewake());
        assert!(HookEvent::PostToolUseFailure.supports_async_rewake());
        assert!(HookEvent::Stop.supports_async_rewake());
        assert!(!HookEvent::PreToolUse.supports_async_rewake());
        assert!(!HookEvent::SessionStart.supports_async_rewake());
        assert!(!HookEvent::PermissionRequest.supports_async_rewake());
    }

    #[test]
    fn event_can_block() {
        assert!(HookEvent::UserPromptSubmit.can_block());
        assert!(HookEvent::PreToolUse.can_block());
        assert!(HookEvent::Stop.can_block());
        assert!(HookEvent::PermissionRequest.can_block());
        assert!(!HookEvent::PostToolUse.can_block());
        assert!(!HookEvent::SessionStart.can_block());
    }

    #[test]
    fn event_can_modify() {
        assert!(HookEvent::PreToolUse.can_modify());
        assert!(HookEvent::PostToolUse.can_modify());
        assert!(HookEvent::PostToolUseFailure.can_modify());
        assert!(!HookEvent::Stop.can_modify());
        assert!(!HookEvent::SessionStart.can_modify());
    }

    #[test]
    fn matcher_matches_event_only() {
        let m = HookMatcher::for_events(vec![HookEvent::PreToolUse, HookEvent::PostToolUse]);
        assert!(m.matches_event(HookEvent::PreToolUse));
        assert!(m.matches_event(HookEvent::PostToolUse));
        assert!(!m.matches_event(HookEvent::Stop));
    }

    #[test]
    fn matcher_matches_all_tools_when_none() {
        let m = HookMatcher::for_events(vec![HookEvent::PreToolUse]);
        assert!(m.matches(HookEvent::PreToolUse, Some("fs.write")));
        assert!(m.matches(HookEvent::PreToolUse, Some("shell.run")));
    }

    #[test]
    fn matcher_matches_specific_tools() {
        let m = HookMatcher::for_tools(
            vec![HookEvent::PreToolUse],
            vec!["fs.write".to_string(), "fs.edit".to_string()],
        );
        assert!(m.matches(HookEvent::PreToolUse, Some("fs.write")));
        assert!(m.matches(HookEvent::PreToolUse, Some("fs.edit")));
        assert!(!m.matches(HookEvent::PreToolUse, Some("shell.run")));
    }

    #[test]
    fn matcher_glob_star() {
        let m = HookMatcher::for_tools(vec![HookEvent::PreToolUse], vec!["fs.*".to_string()]);
        assert!(m.matches(HookEvent::PreToolUse, Some("fs.write")));
        assert!(m.matches(HookEvent::PreToolUse, Some("fs.read")));
        assert!(m.matches(HookEvent::PreToolUse, Some("fs.list")));
        assert!(!m.matches(HookEvent::PreToolUse, Some("shell.run")));
    }

    #[test]
    fn matcher_glob_pipe() {
        let m = HookMatcher::for_tools(
            vec![HookEvent::PreToolUse],
            vec!["fs.write|fs.edit".to_string()],
        );
        assert!(m.matches(HookEvent::PreToolUse, Some("fs.write")));
        assert!(m.matches(HookEvent::PreToolUse, Some("fs.edit")));
        assert!(!m.matches(HookEvent::PreToolUse, Some("fs.read")));
    }

    #[test]
    fn matcher_non_tool_event_ignores_tool_filter() {
        // SessionStart 是非工具事件，tools 过滤应被忽略。
        let m = HookMatcher::for_tools(vec![HookEvent::SessionStart], vec!["fs.write".to_string()]);
        assert!(m.matches(HookEvent::SessionStart, None));
        assert!(m.matches(HookEvent::SessionStart, Some("anything")));
    }

    #[test]
    fn glob_match_exact() {
        assert!(single_glob_match("fs.write", "fs.write"));
        assert!(!single_glob_match("fs.write", "fs.read"));
    }

    #[test]
    fn glob_match_wildcard() {
        assert!(single_glob_match("fs.*", "fs.write"));
        assert!(single_glob_match("fs.*", "fs.read"));
        assert!(!single_glob_match("fs.*", "shell.run"));
        assert!(single_glob_match("*", "anything"));
        assert!(single_glob_match("*.rs", "main.rs"));
        assert!(!single_glob_match("*.rs", "main.ts"));
    }

    #[test]
    fn glob_match_pipe() {
        assert!(glob_match("fs.write|fs.edit", "fs.write"));
        assert!(glob_match("fs.write|fs.edit", "fs.edit"));
        assert!(!glob_match("fs.write|fs.edit", "fs.read"));
    }

    #[test]
    fn hook_output_continue_default() {
        let out = HookOutput::continue_();
        assert_eq!(out.decision, HookDecision::Continue);
        assert!(out.reason.is_none());
        assert!(out.modify_input.is_none());
        assert!(out.inject_context.is_none());
    }

    #[test]
    fn hook_output_allow_with_reason() {
        let out = HookOutput::allow("auto-approved");
        assert_eq!(out.decision, HookDecision::Allow);
        assert_eq!(out.reason.as_deref(), Some("auto-approved"));
    }

    #[test]
    fn hook_output_deny_with_reason() {
        let out = HookOutput::deny("blocked by hook");
        assert_eq!(out.decision, HookDecision::Deny);
        assert_eq!(out.reason.as_deref(), Some("blocked by hook"));
    }

    #[test]
    fn hook_output_inject() {
        let out = HookOutput::inject("sprint context");
        assert_eq!(out.decision, HookDecision::Continue);
        assert_eq!(out.inject_context.as_deref(), Some("sprint context"));
    }

    #[test]
    fn noop_registry_returns_empty() {
        let reg = NoopHookRegistry;
        assert!(reg.for_event(HookEvent::PreToolUse).is_empty());
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn hook_input_new_minimal() {
        let input = HookInput::new(
            HookEvent::SessionStart,
            "sess_01H",
            1,
            Utf8PathBuf::from("/tmp"),
        );
        assert_eq!(input.event, HookEvent::SessionStart);
        assert_eq!(input.turn, 1);
        assert!(input.tool.is_none());
        assert!(input.side_effect.is_none());
        assert!(input.verdict.is_none());
    }

    /// 测试用 Hook：固定返回指定 `HookOutput`。
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
        fn run(&self, _input: HookInput) -> BoxFuture<'_, Result<HookOutput, HookError>> {
            let out = self.output.clone();
            Box::pin(async move { Ok(out) })
        }
    }

    // ===== Default impls =====

    #[test]
    fn on_hook_error_default_is_continue() {
        assert_eq!(OnHookError::default(), OnHookError::Continue);
    }

    #[test]
    fn hook_decision_default_is_continue() {
        assert_eq!(HookDecision::default(), HookDecision::Continue);
    }

    #[test]
    fn dispatch_config_default_values() {
        let cfg = DispatchConfig::default();
        assert_eq!(cfg.on_error, OnHookError::Continue);
        assert_eq!(cfg.timeout, std::time::Duration::from_secs(30));
        assert!(cfg.builtin_deny.is_none());
    }

    #[test]
    fn dispatch_result_default_is_empty() {
        let r = DispatchResult::default();
        assert_eq!(r.decision, HookDecision::Continue);
        assert!(r.reason.is_none());
        assert!(r.modify_input.is_none());
        assert!(
            r.inject_contexts.is_empty(),
            "expected empty: r.inject_contexts"
        );
        assert!(
            r.exit_messages.is_empty(),
            "expected empty: r.exit_messages"
        );
        assert!(r.async_rewake.is_none());
        assert!(r.errors.is_empty(), "expected empty: r.errors");
        assert!(r.fatal_error.is_none());
    }

    #[test]
    fn hook_output_default_is_continue() {
        let out = HookOutput::default();
        assert_eq!(out.decision, HookDecision::Continue);
        assert!(out.reason.is_none());
    }

    // ===== Serde 覆盖 =====

    #[test]
    fn on_hook_error_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&OnHookError::Continue).expect("ser"),
            "\"continue\""
        );
        assert_eq!(
            serde_json::to_string(&OnHookError::Deny).expect("ser"),
            "\"deny\""
        );
        assert_eq!(
            serde_json::to_string(&OnHookError::Fail).expect("ser"),
            "\"fail\""
        );
    }

    #[test]
    fn hook_decision_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&HookDecision::Allow).expect("ser"),
            "\"allow\""
        );
        assert_eq!(
            serde_json::to_string(&HookDecision::Deny).expect("ser"),
            "\"deny\""
        );
        assert_eq!(
            serde_json::to_string(&HookDecision::Ask).expect("ser"),
            "\"ask\""
        );
        assert_eq!(
            serde_json::to_string(&HookDecision::Continue).expect("ser"),
            "\"continue\""
        );
    }

    #[test]
    fn async_rewake_spec_serde_round_trip() {
        let spec = AsyncRewakeSpec {
            estimated_duration_sec: 30,
            description: "cargo audit".to_string(),
        };
        let json = serde_json::to_string(&spec).expect("ser");
        let back: AsyncRewakeSpec = serde_json::from_str(&json).expect("de");
        assert_eq!(back.estimated_duration_sec, 30);
        assert_eq!(back.description, "cargo audit");
    }

    #[test]
    fn hook_input_serde_round_trip_minimal() {
        let input = HookInput::new(
            HookEvent::SessionStart,
            "sess-1",
            1,
            Utf8PathBuf::from("/tmp"),
        );
        let json = serde_json::to_string(&input).expect("ser");
        let back: HookInput = serde_json::from_str(&json).expect("de");
        assert_eq!(back.event, HookEvent::SessionStart);
        assert_eq!(back.session_id, "sess-1");
        assert_eq!(back.turn, 1);
        assert!(back.tool.is_none());
        assert!(back.side_effect.is_none());
        assert!(back.verdict.is_none());
    }

    #[test]
    fn hook_input_serde_round_trip_full() {
        let mut input = HookInput::new(
            HookEvent::PreToolUse,
            "sess-1",
            1,
            Utf8PathBuf::from("/tmp"),
        );
        input.tool = Some(ToolCall {
            id: "c1".to_string(),
            name: "fs.write".to_string(),
            input: serde_json::json!({"path": "main.rs"}),
        });
        input.side_effect = Some(SideEffect::FileWrite);
        input.verdict = Some(VerdictSerde::Ask {
            tool: "fs.write".to_string(),
            summary: "writing main.rs".to_string(),
        });
        input.extras = serde_json::json!({"tokens_before": 100});
        let json = serde_json::to_string(&input).expect("ser");
        let back: HookInput = serde_json::from_str(&json).expect("de");
        assert_eq!(back.event, HookEvent::PreToolUse);
        assert!(back.tool.is_some());
        assert_eq!(back.side_effect, Some(SideEffect::FileWrite));
        assert!(back.verdict.is_some());
        assert_eq!(back.extras["tokens_before"], 100);
    }

    #[test]
    fn hook_output_serde_round_trip_full() {
        let out = HookOutput {
            decision: HookDecision::Deny,
            reason: Some("blocked".to_string()),
            modify_input: Some(serde_json::json!({"path": "alt.rs"})),
            inject_context: Some("ctx".to_string()),
            exit_message: Some("bye".to_string()),
            async_rewake: Some(AsyncRewakeSpec {
                estimated_duration_sec: 5,
                description: "task".to_string(),
            }),
        };
        let json = serde_json::to_string(&out).expect("ser");
        let back: HookOutput = serde_json::from_str(&json).expect("de");
        assert_eq!(back.decision, HookDecision::Deny);
        assert_eq!(back.reason.as_deref(), Some("blocked"));
        assert!(back.modify_input.is_some());
        assert_eq!(back.inject_context.as_deref(), Some("ctx"));
        assert_eq!(back.exit_message.as_deref(), Some("bye"));
        assert!(back.async_rewake.is_some());
    }

    #[test]
    fn hook_output_skip_serializing_none_fields() {
        let out = HookOutput::continue_();
        let json = serde_json::to_string(&out).expect("ser");
        assert!(!json.contains("reason"));
        assert!(!json.contains("modify_input"));
        assert!(!json.contains("inject_context"));
        assert!(!json.contains("exit_message"));
        assert!(!json.contains("async_rewake"));
    }

    #[test]
    fn hook_output_deserialize_missing_decision_uses_default() {
        // 空对象反序列化时，decision 用 default_decision() = Continue
        let json = r#"{}"#;
        let out: HookOutput = serde_json::from_str(json).expect("de");
        assert_eq!(out.decision, HookDecision::Continue);
    }

    // ===== NoopHookRegistry register =====

    #[test]
    fn noop_registry_register_is_noop() {
        let reg = NoopHookRegistry;
        let hook: Arc<dyn Hook> = Arc::new(StaticHook {
            name: "test".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::continue_(),
        });
        reg.register(hook);
        assert_eq!(reg.count(), 0);
        assert!(reg.for_event(HookEvent::PreToolUse).is_empty());
    }

    // ===== for_event_with_tool 默认实现 =====

    /// 仅实现必需方法的注册表（测 trait 默认 `for_event_with_tool`；
    /// `TestRegistry` 已随 dispatch 算法迁往 `minicoding-hooks`）。
    struct MinimalRegistry {
        hooks: std::sync::Mutex<Vec<Arc<dyn Hook>>>,
    }
    impl MinimalRegistry {
        fn new() -> Self {
            Self {
                hooks: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    impl HookRegistry for MinimalRegistry {
        fn register(&self, hook: Arc<dyn Hook>) {
            self.hooks.lock().unwrap().push(hook);
        }
        fn for_event(&self, event: HookEvent) -> Vec<Arc<dyn Hook>> {
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
        /// 测试不触达 dispatch（下沉到 hooks crate 的算法由彼处覆盖）。
        fn dispatch(
            &self,
            _input: HookInput,
            _config: DispatchConfig,
        ) -> crate::provider::BoxFuture<'_, DispatchResult> {
            unreachable!("for_event_with_tool 测试不调用 dispatch");
        }
    }

    #[test]
    fn for_event_with_tool_filters_by_matcher() {
        let reg = MinimalRegistry::new();
        reg.register(Arc::new(StaticHook {
            name: "fs-hook".to_string(),
            matcher: HookMatcher::for_tools(
                vec![HookEvent::PreToolUse],
                vec!["fs.write".to_string()],
            ),
            output: HookOutput::continue_(),
        }));
        reg.register(Arc::new(StaticHook {
            name: "all-hook".to_string(),
            matcher: HookMatcher::for_events(vec![HookEvent::PreToolUse]),
            output: HookOutput::continue_(),
        }));
        // for_event 返回所有订阅该事件的 hook
        assert_eq!(reg.for_event(HookEvent::PreToolUse).len(), 2);
        // tool_name=None：matcher 对工具事件 None 时放行
        let with_none = reg.for_event_with_tool(HookEvent::PreToolUse, None);
        assert_eq!(with_none.len(), 2);
        // fs.write 匹配两个 hook（fs-hook 显式匹配，all-hook 匹配所有）
        let with_fs = reg.for_event_with_tool(HookEvent::PreToolUse, Some("fs.write"));
        assert_eq!(with_fs.len(), 2);
        // shell.run 仅匹配 all-hook（fs-hook 不匹配）
        let with_shell = reg.for_event_with_tool(HookEvent::PreToolUse, Some("shell.run"));
        assert_eq!(with_shell.len(), 1);
        assert_eq!(with_shell[0].name(), "all-hook");
    }

    // ===== glob_match 边界 =====

    #[test]
    fn glob_match_single_star_only() {
        // "*" 匹配任意字符串（含空串）
        assert!(single_glob_match("*", "anything"));
        assert!(single_glob_match("*", ""));
    }

    #[test]
    fn glob_match_multiple_stars() {
        assert!(single_glob_match("a*c*e", "abcde"));
        assert!(single_glob_match("a*c*e", "abbbcccde"));
        assert!(!single_glob_match("a*c*e", "abcd"));
        assert!(single_glob_match("a*b*c", "axxbyyyc"));
    }

    #[test]
    fn glob_match_pipe_with_whitespace() {
        // | 分隔含空白：trim 后匹配
        assert!(glob_match("fs.write | fs.edit", "fs.write"));
        assert!(glob_match("fs.write | fs.edit", "fs.edit"));
        assert!(!glob_match("fs.write | fs.edit", "fs.read"));
    }

    // ===== matcher 构造器与边界 =====

    #[test]
    fn matcher_for_tools_constructor() {
        let m = HookMatcher::for_tools(vec![HookEvent::PostToolUse], vec!["shell.run".to_string()]);
        assert_eq!(m.events.len(), 1);
        assert!(m.tools.is_some());
        assert_eq!(m.tools.as_ref().expect("tools").len(), 1);
    }

    #[test]
    fn matcher_matches_unsubscribed_event() {
        let m = HookMatcher::for_events(vec![HookEvent::PreToolUse]);
        // 未订阅 PostToolUse → 不匹配
        assert!(!m.matches(HookEvent::PostToolUse, Some("fs.write")));
    }
}
