//! `Hook` trait + `HookRegistry` trait + 事件/输入输出 DTO。

use crate::metrics;
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
    fn dispatch(
        &self,
        mut input: HookInput,
        config: DispatchConfig,
    ) -> BoxFuture<'_, DispatchResult> {
        Box::pin(async move {
            let event = input.event;
            let tool_name = input.tool.as_ref().map(|t| t.name.as_str());
            let hooks = self.for_event_with_tool(event, tool_name);

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
                let _enter = span.enter();
                let hook_timer = metrics::start_timer();

                match run_hook_once(hook.as_ref(), &input, &config).await {
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
                            result.inject_contexts.push(ctx);
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
                            result.errors.push((hook_name, e));
                        }
                        HookErrorAction::Deny(reason, e) => {
                            tracing::warn!(hook = %hook_name, error = %e, "hook error -> deny");
                            metrics::record_hook(&hook_name, event_str, "deny");
                            metrics::record_error("hook");
                            result.decision = HookDecision::Deny;
                            result.reason = Some(reason);
                            result.errors.push((hook_name, e));
                            break;
                        }
                        HookErrorAction::Fatal(e) => {
                            tracing::error!(hook = %hook_name, error = %e, "hook error -> fail");
                            metrics::record_hook(&hook_name, event_str, "fatal");
                            metrics::record_error("hook");
                            result.fatal_error = Some(e);
                            return result;
                        }
                    },
                }
            }

            result
        })
    }
}

impl dyn HookRegistry {
    // `merge_decision` 与 `run_hook_once` 作为自由函数定义在下方。
}

/// 单次 Hook 执行的错误处置动作（`run_hook_once` 失败时返回）。
enum HookErrorAction {
    /// `on_error=Continue`：收集错误继续下个 Hook。
    Continue(HookError),
    /// `on_error=Deny`：阻断当前操作。
    Deny(String, HookError),
    /// `on_error=Fail`：致命错误中止 turn。
    Fatal(HookError),
}

impl HookErrorAction {
    /// 按策略把 Hook 错误转为处置动作。
    fn from_error(e: HookError, name: &str, config: &DispatchConfig) -> Self {
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
async fn run_hook_once(
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

/// 合并单个 Hook 的决策到聚合结果（trait 默认 `dispatch` 内部用）。
///
/// 规则见 `HookRegistry::dispatch` 文档。
fn merge_decision(
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

/// 空实现（未启用 hooks feature 时兜底）。
#[derive(Debug, Default, Clone)]
pub struct NoopHookRegistry;

impl HookRegistry for NoopHookRegistry {
    fn register(&self, _hook: Arc<dyn Hook>) {
        // no-op
    }

    fn for_event(&self, _event: HookEvent) -> Vec<Arc<dyn Hook>> {
        Vec::new()
    }

    fn count(&self) -> usize {
        0
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

    // ===== dispatch 聚合测试（T-M5-1）=====

    /// 测试用 `HookRegistry` 实现（core 内验证 `dispatch` 默认实现）。
    struct TestRegistry {
        hooks: std::sync::Mutex<Vec<Arc<dyn Hook>>>,
    }

    impl TestRegistry {
        fn new() -> Self {
            Self {
                hooks: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn register(&self, hook: Arc<dyn Hook>) {
            self.hooks.lock().unwrap().push(hook);
        }
    }

    impl HookRegistry for TestRegistry {
        fn register(&self, hook: Arc<dyn Hook>) {
            TestRegistry::register(self, hook);
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

    /// 测试用 Hook：返回错误（模拟 Hook 失败）。
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
        assert_eq!(result.inject_contexts[0], "git status output");
        assert_eq!(result.inject_contexts[1], "todo list");
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

    #[test]
    fn event_as_str_all_variants() {
        assert_eq!(HookEvent::SessionStart.as_str(), "session_start");
        assert_eq!(HookEvent::UserPromptSubmit.as_str(), "user_prompt_submit");
        assert_eq!(HookEvent::PreToolUse.as_str(), "pre_tool_use");
        assert_eq!(HookEvent::PostToolUse.as_str(), "post_tool_use");
        assert_eq!(
            HookEvent::PostToolUseFailure.as_str(),
            "post_tool_use_failure"
        );
        assert_eq!(HookEvent::PreCompact.as_str(), "pre_compact");
        assert_eq!(HookEvent::PostCompact.as_str(), "post_compact");
        assert_eq!(HookEvent::Stop.as_str(), "stop");
        assert_eq!(HookEvent::SubagentStop.as_str(), "subagent_stop");
        assert_eq!(HookEvent::PermissionRequest.as_str(), "permission_request");
    }

    #[test]
    fn event_is_tool_event_all_variants() {
        assert!(HookEvent::PreToolUse.is_tool_event());
        assert!(HookEvent::PostToolUse.is_tool_event());
        assert!(HookEvent::PostToolUseFailure.is_tool_event());
        assert!(HookEvent::PermissionRequest.is_tool_event());
        assert!(!HookEvent::SessionStart.is_tool_event());
        assert!(!HookEvent::UserPromptSubmit.is_tool_event());
        assert!(!HookEvent::PreCompact.is_tool_event());
        assert!(!HookEvent::PostCompact.is_tool_event());
        assert!(!HookEvent::Stop.is_tool_event());
        assert!(!HookEvent::SubagentStop.is_tool_event());
    }

    #[test]
    fn event_can_block_all_variants() {
        assert!(HookEvent::UserPromptSubmit.can_block());
        assert!(HookEvent::PreToolUse.can_block());
        assert!(HookEvent::Stop.can_block());
        assert!(HookEvent::PermissionRequest.can_block());
        assert!(!HookEvent::SessionStart.can_block());
        assert!(!HookEvent::PostToolUse.can_block());
        assert!(!HookEvent::PostToolUseFailure.can_block());
        assert!(!HookEvent::PreCompact.can_block());
        assert!(!HookEvent::PostCompact.can_block());
        assert!(!HookEvent::SubagentStop.can_block());
    }

    #[test]
    fn event_can_modify_all_variants() {
        assert!(HookEvent::PreToolUse.can_modify());
        assert!(HookEvent::PostToolUse.can_modify());
        assert!(HookEvent::PostToolUseFailure.can_modify());
        assert!(!HookEvent::SessionStart.can_modify());
        assert!(!HookEvent::UserPromptSubmit.can_modify());
        assert!(!HookEvent::PreCompact.can_modify());
        assert!(!HookEvent::PostCompact.can_modify());
        assert!(!HookEvent::Stop.can_modify());
        assert!(!HookEvent::SubagentStop.can_modify());
        assert!(!HookEvent::PermissionRequest.can_modify());
    }

    #[test]
    fn event_supports_async_rewake_all_variants() {
        assert!(HookEvent::PostToolUse.supports_async_rewake());
        assert!(HookEvent::PostToolUseFailure.supports_async_rewake());
        assert!(HookEvent::Stop.supports_async_rewake());
        assert!(!HookEvent::SessionStart.supports_async_rewake());
        assert!(!HookEvent::UserPromptSubmit.supports_async_rewake());
        assert!(!HookEvent::PreToolUse.supports_async_rewake());
        assert!(!HookEvent::PreCompact.supports_async_rewake());
        assert!(!HookEvent::PostCompact.supports_async_rewake());
        assert!(!HookEvent::SubagentStop.supports_async_rewake());
        assert!(!HookEvent::PermissionRequest.supports_async_rewake());
    }

    #[test]
    fn hook_event_serde_pascal_case() {
        let json = serde_json::to_string(&HookEvent::SessionStart).expect("ser");
        assert_eq!(json, "\"SessionStart\"");
        let json = serde_json::to_string(&HookEvent::PostToolUseFailure).expect("ser");
        assert_eq!(json, "\"PostToolUseFailure\"");
        let json = serde_json::to_string(&HookEvent::PermissionRequest).expect("ser");
        assert_eq!(json, "\"PermissionRequest\"");
        // round-trip
        let event: HookEvent = serde_json::from_str("\"PreToolUse\"").expect("de");
        assert_eq!(event, HookEvent::PreToolUse);
        let event: HookEvent = serde_json::from_str("\"SubagentStop\"").expect("de");
        assert_eq!(event, HookEvent::SubagentStop);
    }

    // ===== VerdictSerde 序列化 =====

    #[test]
    fn verdict_serde_allow() {
        let v = VerdictSerde::Allow;
        let json = serde_json::to_string(&v).expect("ser");
        assert_eq!(json, "{\"kind\":\"allow\"}");
        let back: VerdictSerde = serde_json::from_str(&json).expect("de");
        assert!(matches!(back, VerdictSerde::Allow));
    }

    #[test]
    fn verdict_serde_deny() {
        let v = VerdictSerde::Deny {
            reason: "blocked".to_string(),
        };
        let json = serde_json::to_string(&v).expect("ser");
        assert_eq!(json, "{\"kind\":\"deny\",\"reason\":\"blocked\"}");
    }

    #[test]
    fn verdict_serde_ask() {
        let v = VerdictSerde::Ask {
            tool: "fs.write".to_string(),
            summary: "writing file".to_string(),
        };
        let json = serde_json::to_string(&v).expect("ser");
        assert!(json.contains("\"kind\":\"ask\""));
        assert!(json.contains("\"tool\":\"fs.write\""));
        assert!(json.contains("\"summary\":\"writing file\""));
    }

    // ===== HookError Display =====

    #[test]
    fn hook_error_display_timeout() {
        let e = HookError::Timeout {
            name: "h1".to_string(),
            timeout_sec: 5,
        };
        assert_eq!(e.to_string(), "hook `h1` 超时（5s）");
    }

    #[test]
    fn hook_error_display_exit_code() {
        let e = HookError::ExitCode {
            name: "h2".to_string(),
            code: 1,
            stderr: "fail".to_string(),
        };
        assert_eq!(e.to_string(), "hook `h2` 退出码 1: fail");
    }

    #[test]
    fn hook_error_display_invalid_output() {
        let e = HookError::InvalidOutput {
            name: "h3".to_string(),
            reason: "bad json".to_string(),
        };
        assert_eq!(e.to_string(), "hook `h3` 输出解析失败: bad json");
    }

    #[test]
    fn hook_error_display_internal() {
        let e = HookError::Internal("boom".to_string());
        assert_eq!(e.to_string(), "hook 内部错误: boom");
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

    #[test]
    fn for_event_with_tool_filters_by_matcher() {
        let reg = TestRegistry::new();
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

    #[tokio::test]
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

    #[tokio::test]
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
}
