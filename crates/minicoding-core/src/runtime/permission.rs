//! 权限决策 + Hook 管道（A-2026-08 自 rt.rs 抽出，见 `design.md` §9、`hooks.md` §4）。
//!
//! 副作用工具的完整因果链：策略判定（C-02 黑名单最高优先级）→ `PreToolUse` Hook →
//! 改写输入重查取严（S4/C-21）→ 决策解析（Ask→PermissionRequest Hook→prompter）
//! → 审计落盘（C-01）→ 执行/拒绝。Hook 可阻断/改写/升级，但不可覆盖内置黑名单
//! Deny（C-21：合并 verdict 取严 + `builtin_deny` 透传 dispatch 双重保障）。

use super::Event;
use super::rt::Runtime;
use crate::hooks::{DispatchConfig, HookDecision, HookEvent, HookInput, VerdictSerde};
use crate::metrics;
use crate::model::{RuntimeError, SideEffect, ToolCall, ToolCallId, ToolResult};
use crate::otel::span_name;
use crate::policy::{Decision, PermissionContext, Verdict};
use std::time::Duration;

/// S4：合并两个 Verdict 取较严者（Deny > Ask > Allow）。
///
/// 用途：Hook `modify_input` 改写工具入参后，原始输入与修改后输入的策略判定
/// 需同时满足（任一 Deny 即 Deny；任一要求 Ask 则升级为 Ask）。
fn merge_verdicts_stricter(a: &Verdict, b: Verdict) -> Verdict {
    use Verdict::{Allow, Ask, Deny};
    fn rank(v: &Verdict) -> u8 {
        match v {
            Allow => 0,
            Ask(_) => 1,
            Deny(_) => 2,
        }
    }
    if rank(a) >= rank(&b) {
        match a {
            Deny(_) | Ask(_) => a.clone(),
            Allow => b,
        }
    } else {
        b
    }
}

impl Runtime {
    /// 执行一个副作用工具调用（权限链完整入口，C-01）。
    ///
    /// 流程：
    /// 1. 策略判定（`policy.check`，C-02 内置黑名单优先级最高不可覆盖）
    /// 2. 构建 Hook 分发配置
    /// 3. `PreToolUse` Hook（可阻断/改写/直出决策）
    /// 4. Hook 改写输入时对**修改后**输入重跑策略检查并与原 verdict 取严（S4/C-21）
    /// 5. 解析最终决策（Verdict / Hook 直出 / Ask→PermissionRequest Hook→prompter）
    /// 6. 落审计（所有副作用权限决策均落盘）
    /// 7. 执行成功 → `PostToolUse` Hook；执行失败 → `PostToolUseFailure` Hook
    ///
    /// `Deny`（策略/Hook/用户）返回 `Ok` 带 `is_error=true` 的结果；仅 `dispatch`
    /// 失败返回 `Err`（与原 `?` 传播语义一致）。
    #[allow(clippy::too_many_lines)] // 权限决策 + Hook + 执行 + 审计 + 详细日志，拆分反而降低因果链可读性
    pub(crate) async fn execute_side_effect_call(
        &self,
        call: &ToolCall,
        ctx: &crate::tool::ToolContext,
    ) -> Result<(ToolCallId, ToolResult), RuntimeError> {
        let side_effect = self
            .tools
            .get(&call.name)
            .map_or(SideEffect::None, |t| t.side_effect());

        // `permission` span（design.md §15.1）：包裹权限决策流程（策略判定 →
        // Hook → prompter 交互 → 审计落盘）。字段不含 input 原文（C-04）。
        // `permission.verdict` 在决策确定后通过 `Span::current().record()` 填充。
        let span = tracing::info_span!(
            "permission",
            session.id = %self.session.id,
            tool.name = %call.name,
            tool.side_effect = ?side_effect,
            permission.verdict = tracing::field::Empty,
            otel.name = span_name::PERMISSION_CHECK,
        );
        let _enter = span.enter();
        tracing::info!(
            tool.name = %call.name,
            call_id = %call.id,
            phase = "start",
            "tool_call started (side effect)"
        );

        let plan_snap = self.plan_state.read().await.clone();
        let perm_ctx = PermissionContext {
            session: self.session.id.clone(),
            workdir: self.workdir.read().await.clone(),
            side_effect,
            turn: self.current_turn.load(std::sync::atomic::Ordering::Relaxed),
            // S23：近期权限决策摘要（最近 turn 的 tool_calls 映射为 Allow 决策）
            history: {
                let mut hist = Vec::new();
                for msg in self.session.messages.iter().rev() {
                    if hist.len() >= 5 {
                        break;
                    }
                    for tc in &msg.tool_calls {
                        if hist.len() >= 5 {
                            break;
                        }
                        let input_str = serde_json::to_string(&tc.input).unwrap_or_default();
                        let summary =
                            format!("{}({})", tc.name, &input_str[..input_str.len().min(80)]);
                        hist.push(Decision::Allow); // 近期已 Allow 的决策记录
                        let _ = summary; // Decision 无 detail 字段，保留供未来扩展
                    }
                }
                hist
            },
            permission_mode: plan_snap.mode,
            allowed_prompts: plan_snap.allowed_prompts,
        };

        // 1. 策略判定（C-02：内置黑名单在此优先级最高，不可覆盖）
        let verdict = match self.policy.check(&call.name, &call.input, &perm_ctx).await {
            Ok(v) => v,
            Err(e) => {
                self.record_permission_audit(&call.name, &Decision::Deny(e.to_string()), None)
                    .await;
                tracing::warn!(tool = %call.name, error = %e, "policy check failed");
                return Ok((
                    call.id.clone(),
                    ToolResult::err_text(format!("permission error: {e}")),
                ));
            }
        };

        // 2. 构建 Hook 分发配置（S4：Hook 改写输入后此处会基于合并 verdict 重建）
        let dispatch_cfg = self.build_dispatch_config(&verdict);

        // 3. PreToolUse Hook（policy.check 之后、工具执行前）
        let mut effective_call = call.clone();
        let pre_decision = self
            .run_pre_tool_use_hook(
                call,
                side_effect,
                &verdict,
                &dispatch_cfg,
                &mut effective_call,
            )
            .await?;

        // 3.1 S4/C-01/C-21：Hook `modify_input` 修改了输入时，对**修改后**的输入重跑
        //     策略检查并与原 verdict 取严（Deny > Ask > Allow）——用户批准的是原始
        //     输入，Hook 改写后的输入必须重新过黑名单/路径策略，否则批准 A 执行 B。
        let input_modified = effective_call.input != call.input;
        let verdict = if input_modified {
            match self
                .policy
                .check(&call.name, &effective_call.input, &perm_ctx)
                .await
            {
                Ok(rechecked) => merge_verdicts_stricter(&verdict, rechecked),
                Err(e) => {
                    self.record_permission_audit(&call.name, &Decision::Deny(e.to_string()), None)
                        .await;
                    tracing::warn!(tool = %call.name, error = %e, "policy recheck on modified input failed");
                    return Ok((
                        call.id.clone(),
                        ToolResult::err_text(format!("permission error: {e}")),
                    ));
                }
            }
        } else {
            verdict
        };
        // 合并后 verdict 可能升级为 Deny：重建 dispatch_cfg/is_builtin_deny，
        // 保证 C-21（builtin Deny 不被 Hook Allow 覆盖）对改写后输入同样成立
        let dispatch_cfg = if input_modified {
            self.build_dispatch_config(&verdict)
        } else {
            dispatch_cfg
        };
        let is_builtin_deny = matches!(verdict, Verdict::Deny(_));

        // PreToolUse 直出决策与合并 verdict 冲突时取严（Hook Allow 不能越过重查 Deny）
        let pre_decision = match (&pre_decision, &verdict) {
            (Some(Decision::Allow), Verdict::Deny(reason)) => Some(Decision::Deny(format!(
                "输入被 Hook 修改后未通过策略复查: {reason}"
            ))),
            _ => pre_decision,
        };

        // 4. 解析为最终决策（PreToolUse 直出 / Verdict Allow|Deny / Ask→PermissionRequest Hook→prompter）。
        //    Ask 场景传 effective_call——弹窗展示的是实际将执行的（可能被 Hook 改写的）输入。
        let (decision, prompt_id) = if let Some(d) = pre_decision {
            (d, None)
        } else {
            self.resolve_decision(
                &verdict,
                if input_modified {
                    &effective_call
                } else {
                    call
                },
                side_effect,
                &dispatch_cfg,
                is_builtin_deny,
            )
            .await?
        };

        // 5. 落审计（所有副作用权限决策均落盘，AGENTS.md §5.5；
        //    C-04：detail 不含工具输入原文，避免凭证外泄）
        self.record_permission_audit(&call.name, &decision, prompt_id)
            .await;

        // Metrics: 记录权限决策
        let verdict_str = match &decision {
            Decision::Allow => "allow",
            Decision::Deny(_) => "deny",
        };
        metrics::record_permission(verdict_str);

        // Span 属性：动态记录最终 verdict（决策在 span 创建后才确定）
        tracing::Span::current().record("permission.verdict", verdict_str);

        // 6. 按决策执行或拒绝
        let side_effect_str = match side_effect {
            SideEffect::None => "none",
            SideEffect::FileWrite => "file_write",
            SideEffect::Command => "command",
            SideEffect::Network => "network",
        };
        let tool_timer = metrics::start_timer();
        let result = match decision {
            Decision::Deny(msg) => Ok((
                call.id.clone(),
                ToolResult::err_text(format!("permission denied: {msg}")),
            )),
            Decision::Allow => {
                self.execute_allowed_call(call, &effective_call, side_effect, ctx)
                    .await
            }
        };
        // Metrics: 记录副作用工具调用
        let result_str = match &result {
            Ok((_, r)) if r.is_error => "err",
            Ok(_) => "ok",
            Err(_) => "err",
        };
        // 详细日志：副作用工具执行结果（含权限决策）
        match &result {
            Ok((_, r)) => {
                tracing::info!(
                    tool.name = %call.name,
                    call_id = %call.id,
                    tool.elapsed_ms = u64::try_from(tool_timer.elapsed().as_millis()).unwrap_or(u64::MAX),
                    tool.is_error = r.is_error,
                    tool.output_bytes = r.metadata.bytes,
                    permission.verdict = verdict_str,
                    phase = "finish",
                    "tool_call finished (side effect)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    tool.name = %call.name,
                    call_id = %call.id,
                    tool.elapsed_ms = u64::try_from(tool_timer.elapsed().as_millis()).unwrap_or(u64::MAX),
                    error = %e,
                    permission.verdict = verdict_str,
                    phase = "finish",
                    "tool_call failed"
                );
            }
        }
        metrics::record_tool_call(&call.name, side_effect_str, result_str);
        metrics::record_elapsed("tool_call_duration_ms", "tool", &call.name, tool_timer);
        if result.is_err() {
            metrics::record_error("tool");
        }
        result
    }

    /// 构建 Hook 分发配置（来自 `HooksConfig` + 当前 `Verdict`）。
    ///
    /// C-21：policy 返回 `Deny` 时视为内置黑名单 Deny（当前 `BuiltinPolicy` 仅产出
    /// L0 Deny：项目文档保护 C-02、路径越界 C-03），Hook 的 Allow 被忽略。
    fn build_dispatch_config(&self, verdict: &Verdict) -> DispatchConfig {
        // 短临界区读取两个 Copy 字段（guard 不跨 await，`&self` 同步 fn 亦可用）
        let (on_error, default_timeout_sec) = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (cfg.hooks.on_hook_error, cfg.hooks.default_timeout_sec)
        };
        let builtin_deny = match verdict {
            Verdict::Deny(msg) => Some(msg.clone()),
            _ => None,
        };
        DispatchConfig {
            on_error,
            timeout: Duration::from_secs(default_timeout_sec),
            builtin_deny,
        }
    }

    /// 运行 `PreToolUse` Hook（policy.check 之后、工具执行前）。
    ///
    /// 返回 `Some(Decision)` 表示 Hook 直接给出决策（Deny 或 Allow 升级）；
    /// 返回 `None` 表示 Hook 未决策（`Continue`/`Ask`/`Allow` on `builtin_deny`），由
    /// 调用方继续走 `resolve_decision` 解析。
    ///
    /// `effective_call` 在 Hook 返回 `modify_input` 时被就地更新（仍经
    /// `sandbox_path` 校验，由工具 dispatch 时执行）。
    async fn run_pre_tool_use_hook(
        &self,
        call: &ToolCall,
        side_effect: SideEffect,
        verdict: &Verdict,
        dispatch_cfg: &DispatchConfig,
        effective_call: &mut ToolCall,
    ) -> Result<Option<Decision>, RuntimeError> {
        let is_builtin_deny = matches!(verdict, Verdict::Deny(_));
        let hook_input = self
            .build_hook_input(HookEvent::PreToolUse, call, side_effect, Some(verdict))
            .await;
        let result = self
            .hook_registry
            .dispatch(hook_input, dispatch_cfg.clone())
            .await;
        if let Some(fatal) = result.fatal_error {
            return Err(RuntimeError::Hook(fatal.to_string()));
        }
        // C-21：builtin_deny 时 Hook 的 Allow 被忽略（dispatch 已处理）
        let pre_decision = match result.decision {
            HookDecision::Deny => {
                let reason = result
                    .reason
                    .unwrap_or_else(|| "blocked by hook".to_string());
                Some(Decision::Deny(reason))
            }
            HookDecision::Allow if !is_builtin_deny => {
                // Hook 升级 Ask→Allow（不降级已有 Allow）
                Some(Decision::Allow)
            }
            _ => None, // Continue/Ask/Allow(builtin_deny) 不直接给决策
        };
        // 应用 modify_input（仍经 sandbox_path 校验，由工具 dispatch 时执行）
        if let Some(new_input) = result.modify_input {
            effective_call.input = new_input;
        }
        // exit_messages 记日志（供观测）
        for msg in &result.exit_messages {
            tracing::info!(tool = %call.name, hook_msg = %msg, "PreToolUse hook exit message");
        }
        Ok(pre_decision)
    }

    /// 解析为最终决策（PreToolUse 未直出决策时）。
    ///
    /// - `Allow` / `Deny` → 直出
    /// - `Ask` → 先跑 `PermissionRequest` Hook（可能短路）；未短路则走 `prompter`
    ///
    /// 返回 `(Decision, Option<prompt_id>)`：`prompt_id` 为 `Some` 表示经用户交互。
    async fn resolve_decision(
        &self,
        verdict: &Verdict,
        call: &ToolCall,
        side_effect: SideEffect,
        dispatch_cfg: &DispatchConfig,
        is_builtin_deny: bool,
    ) -> Result<(Decision, Option<String>), RuntimeError> {
        match verdict {
            Verdict::Allow => Ok((Decision::Allow, None)),
            Verdict::Deny(msg) => Ok((Decision::Deny(msg.clone()), None)),
            Verdict::Ask(prompt) => {
                // PermissionRequest Hook（Verdict::Ask 时、prompter 前）
                let hook_input = self
                    .build_hook_input(
                        HookEvent::PermissionRequest,
                        call,
                        side_effect,
                        Some(verdict),
                    )
                    .await;
                let result = self
                    .hook_registry
                    .dispatch(hook_input, dispatch_cfg.clone())
                    .await;
                if let Some(fatal) = result.fatal_error {
                    return Err(RuntimeError::Hook(fatal.to_string()));
                }
                match result.decision {
                    HookDecision::Allow if !is_builtin_deny => {
                        // Hook 自动批准，跳过 prompter
                        Ok((Decision::Allow, None))
                    }
                    HookDecision::Deny => {
                        let reason = result
                            .reason
                            .unwrap_or_else(|| "blocked by hook".to_string());
                        Ok((Decision::Deny(reason), None))
                    }
                    _ => {
                        // Hook 未决策 → 走 prompter 交互
                        let prompt_id = prompt.id.clone();
                        self.events.emit(Event::PermissionRequested {
                            id: prompt.id.clone(),
                            tool: prompt.tool.clone(),
                            summary: prompt.summary.clone(),
                            risk: prompt.risk,
                        });
                        let d = self.prompter.prompt(prompt.clone()).await;
                        let event = Event::PermissionResolved {
                            id: prompt_id.clone(),
                            decision: d.clone(),
                        };
                        self.persist_event(&event).await;
                        self.events.emit(event);
                        Ok((d, Some(prompt_id)))
                    }
                }
            }
        }
    }

    /// 执行已 Allow 的工具调用（含沙箱拒绝检测、PostToolUse/PostToolUseFailure Hook）。
    async fn execute_allowed_call(
        &self,
        original_call: &ToolCall,
        effective_call: &ToolCall,
        side_effect: SideEffect,
        ctx: &crate::tool::ToolContext,
    ) -> Result<(ToolCallId, ToolResult), RuntimeError> {
        self.events.emit(Event::ToolCallStarted {
            call_id: original_call.id.clone(),
            tool: original_call.name.clone(),
        });
        let result = match self.tools.dispatch(effective_call, ctx).await {
            Ok(r) => r,
            Err(e) => {
                // 沙箱拒绝检测（T-M4-5）：识别 EPERM/EACCES/landlock 等
                // 内核级硬反馈，更新熔断器（C-30 不可被 LLM 绕过）。
                if let Some(denial_result) =
                    self.handle_sandbox_denial(&original_call.id, &original_call.name, &e)
                {
                    return Ok(denial_result);
                }
                // 沙箱初始化失败（apply/post_spawn，如 Windows Job Object 恢复线程
                // 竞态）：询问用户是否在沙箱外重试一次（C-22 用户显式选定）。
                if let Some(fallback_ctx) =
                    self.maybe_sandbox_fallback(original_call, &e, ctx).await
                {
                    // 沙箱外重试：仅重试一次，不再二次询问（避免询问循环）
                    match self.tools.dispatch(effective_call, &fallback_ctx).await {
                        Ok(r) => r,
                        Err(e2) => {
                            // PostToolUseFailure Hook（重试仍失败，非 denial 错误）
                            self.run_post_failure_hook(effective_call, side_effect, &e2)
                                .await;
                            ToolResult::err_text(format!("tool error: {e2}"))
                        }
                    }
                } else {
                    // PostToolUseFailure Hook（非 denial 错误）
                    self.run_post_failure_hook(effective_call, side_effect, &e)
                        .await;
                    // design.md §4.5：工具错误以 is_error=true 回灌 LLM 自我修正，不中止 turn。
                    ToolResult::err_text(format!("tool error: {e}"))
                }
            }
        };
        // PostToolUse Hook（执行成功后）
        self.run_post_success_hook(effective_call, side_effect, &result)
            .await;
        self.events.emit(Event::ToolCallFinished {
            call_id: original_call.id.clone(),
            result: result.clone(),
        });
        Ok((original_call.id.clone(), result))
    }

    /// 构造 `HookInput`（工具相关事件通用）。
    async fn build_hook_input(
        &self,
        event: HookEvent,
        call: &ToolCall,
        side_effect: SideEffect,
        verdict: Option<&Verdict>,
    ) -> HookInput {
        let verdict_serde = verdict.map(|v| match v {
            Verdict::Allow => VerdictSerde::Allow,
            Verdict::Deny(msg) => VerdictSerde::Deny {
                reason: msg.clone(),
            },
            Verdict::Ask(prompt) => VerdictSerde::Ask {
                tool: prompt.tool.clone(),
                summary: prompt.summary.clone(),
            },
        });
        HookInput {
            event,
            session_id: self.session.id.clone(),
            turn: 0,
            tool: Some(call.clone()),
            side_effect: Some(side_effect),
            verdict: verdict_serde,
            cwd: self.workdir.read().await.clone(),
            extras: serde_json::Value::Null,
        }
    }

    /// 运行 `PostToolUse` Hook（工具执行成功后，见 `hooks.md` §4）。
    ///
    /// Hook 可跑 formatter/linter（副作用在 Hook 内部完成），`exit_message` 记日志。
    /// `async_rewake` 暂不处理（AsyncRewakeManager 集成在后续任务）。
    async fn run_post_success_hook(
        &self,
        call: &ToolCall,
        side_effect: SideEffect,
        _result: &ToolResult,
    ) {
        // 短临界区读取（guard 在首个 await 前释放）+ 快速跳过
        let (on_error, default_timeout_sec, has_post_tool_use) = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                cfg.hooks.on_hook_error,
                cfg.hooks.default_timeout_sec,
                !cfg.hooks.post_tool_use.is_empty(),
            )
        };
        if !has_post_tool_use {
            return; // 无 PostToolUse Hook，快速跳过
        }
        let dispatch_cfg = DispatchConfig {
            on_error,
            timeout: Duration::from_secs(default_timeout_sec),
            builtin_deny: None,
        };
        let hook_input = self
            .build_hook_input(HookEvent::PostToolUse, call, side_effect, None)
            .await;
        let result = self.hook_registry.dispatch(hook_input, dispatch_cfg).await;
        if let Some(fatal) = result.fatal_error {
            tracing::error!(hook_error = %fatal, "PostToolUse hook fatal error");
        }
        for msg in &result.exit_messages {
            tracing::info!(tool = %call.name, hook_msg = %msg, "PostToolUse hook exit message");
        }
    }

    /// 运行 `PostToolUseFailure` Hook（工具执行失败后，见 `hooks.md` §4）。
    ///
    /// Hook 可诊断失败原因、记录错误模式。`exit_message` 记日志。
    async fn run_post_failure_hook(
        &self,
        call: &ToolCall,
        side_effect: SideEffect,
        _error: &crate::model::ToolError,
    ) {
        // 短临界区读取（guard 在首个 await 前释放）+ 快速跳过
        let (on_error, default_timeout_sec, has_post_failure_hook) = {
            let cfg = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                cfg.hooks.on_hook_error,
                cfg.hooks.default_timeout_sec,
                !cfg.hooks.post_tool_use_failure.is_empty(),
            )
        };
        if !has_post_failure_hook {
            return; // 无 PostToolUseFailure Hook，快速跳过
        }
        let dispatch_cfg = DispatchConfig {
            on_error,
            timeout: Duration::from_secs(default_timeout_sec),
            builtin_deny: None,
        };
        let hook_input = self
            .build_hook_input(HookEvent::PostToolUseFailure, call, side_effect, None)
            .await;
        let result = self.hook_registry.dispatch(hook_input, dispatch_cfg).await;
        if let Some(fatal) = result.fatal_error {
            tracing::error!(hook_error = %fatal, "PostToolUseFailure hook fatal error");
        }
        for msg in &result.exit_messages {
            tracing::info!(tool = %call.name, hook_msg = %msg, "PostToolUseFailure hook exit message");
        }
    }

    /// 记录权限决策审计（C-01 决策可追溯，AGENTS.md §5.5）。
    ///
    /// `prompt_id` 为 `Some` 表示经用户交互（Ask→prompter），`None` 表示策略直出
    /// （Allow/Deny）。审计落盘失败仅记 `warn` 日志，不中断工具执行——审计失败不应
    /// 阻断主流程，但会被运维发现并处理。
    pub(crate) async fn record_permission_audit(
        &self,
        tool: &str,
        decision: &Decision,
        prompt_id: Option<String>,
    ) {
        let (decision_str, detail) = match (decision, prompt_id.is_some()) {
            (Decision::Allow, true) => ("allow", format!("user allowed {tool}")),
            (Decision::Allow, false) => ("allow", format!("policy allowed {tool}")),
            (Decision::Deny(reason), true) => ("deny", format!("user denied {tool}: {reason}")),
            (Decision::Deny(reason), false) => ("deny", format!("policy denied {tool}: {reason}")),
        };
        let rec = crate::storage::AuditRecord {
            ts: time::OffsetDateTime::now_utc(),
            session: self.session.id.clone(),
            kind: crate::storage::AuditKind::PermissionResolved,
            tool: Some(tool.to_string()),
            decision: Some(decision_str.to_string()),
            detail,
        };
        if let Err(e) = self.audit.record(rec).await {
            tracing::warn!(error = %e, "audit record failed");
        }
    }
}
