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
use tracing::Instrument;

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
    /// 发送被拒绝调用的生命周期事件对（Started + Finished，2026-08-23 审查 §4-P2）。
    ///
    /// 拒绝/权限错误路径不经过 `execute_allowed_call`（其内含正常的事件发射），
    /// 由本辅助补齐同一语义：SSE 消费者可见"卡片出现 → 以错误结果终结"，
    /// 与只读桶、Allow 路径的生命周期事件保持一致。
    fn emit_denied_lifecycle(&self, call_id: &ToolCallId, tool: &str, result: &ToolResult) {
        self.events.emit(Event::ToolCallStarted {
            call_id: call_id.clone(),
            tool: tool.to_string(),
        });
        self.events.emit(Event::ToolCallFinished {
            call_id: call_id.clone(),
            result: result.clone(),
        });
    }

    /// 派发生命周期 Hook（SessionStart/UserPromptSubmit，遗留#6 全量接线）。
    ///
    /// `inject_context` 收集进 pending 缓冲（下一请求 system 头部）；fatal 错误
    /// 仅 warn——生命周期阶段失败不阻塞会话。
    pub(crate) async fn run_lifecycle_hook(
        &self,
        event: crate::hooks::HookEvent,
        extras: serde_json::Value,
    ) {
        let input = crate::hooks::HookInput {
            event,
            session_id: self.session.id.clone(),
            turn: self.current_turn.load(std::sync::atomic::Ordering::Relaxed),
            tool: None,
            side_effect: None,
            verdict: None,
            cwd: self.workdir.read().await.clone(),
            extras,
        };
        let cfg = {
            let c = self
                .config
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::hooks::DispatchConfig {
                on_error: c.hooks.on_hook_error,
                timeout: std::time::Duration::from_secs(c.hooks.default_timeout_sec),
                builtin_deny: None,
            }
        };
        let result = self.hook_registry.dispatch(input, cfg).await;
        if let Some(fatal) = result.fatal_error {
            tracing::warn!(error = %fatal, "lifecycle hook fatal error (ignored)");
        }
        for ctx_text in &result.inject_contexts {
            self.pending_hook_contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(ctx_text.clone());
        }
    }

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
        //
        // CORE-4（2026-08-25 R2 审查）：用 `.instrument()` 而非 `span.enter()`——
        // `Entered` guard 跨 await 持有会在线程局部 span 上失真（prompter 等待
        // 用户决策可达数百秒），且使 future 非 `Send`。instrument 把 span 绑定
        // 到整个 future，语义与线程无关。
        let span = tracing::info_span!(
            "permission",
            session.id = %self.session.id,
            tool.name = %call.name,
            tool.side_effect = ?side_effect,
            permission.verdict = tracing::field::Empty,
            otel.name = span_name::PERMISSION_CHECK,
        );
        self.execute_side_effect_call_inner(call, ctx, side_effect)
            .instrument(span)
            .await
    }

    /// [`Self::execute_side_effect_call`] 的主体（span 经 instrument 注入）。
    #[allow(clippy::too_many_lines)]
    async fn execute_side_effect_call_inner(
        &self,
        call: &ToolCall,
        ctx: &crate::tool::ToolContext,
        side_effect: SideEffect,
    ) -> Result<(ToolCallId, ToolResult), RuntimeError> {
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
            // S23（reserved）：近期决策历史。运行期消息不回写 `session.messages`
            // （storage 为事实源），真实决策需从 AuditSink 回读——接入前恒为空，
            // 不再填充伪造的 Allow 序列（2026-08-23 审查 §4-P1/P0）。
            history: Vec::new(),
            permission_mode: plan_snap.mode,
            allowed_prompts: plan_snap.allowed_prompts,
        };

        // 1. 策略判定（C-02：内置黑名单在此优先级最高，不可覆盖）
        let verdict = match self.policy.check(&call.name, &call.input, &perm_ctx).await {
            Ok(v) => v,
            Err(e) => {
                let result = ToolResult::err_text(format!("permission error: {e}"));
                self.emit_denied_lifecycle(&call.id, &call.name, &result);
                self.record_permission_audit(
                    &call.name,
                    &Decision::Deny(e.to_string()),
                    None,
                    None,
                )
                .await;
                tracing::warn!(tool = %call.name, error = %e, "policy check failed");
                return Ok((call.id.clone(), result));
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
                    let result = ToolResult::err_text(format!("permission error: {e}"));
                    self.emit_denied_lifecycle(&call.id, &call.name, &result);
                    self.record_permission_audit(
                        &call.name,
                        &Decision::Deny(e.to_string()),
                        None,
                        None,
                    )
                    .await;
                    tracing::warn!(tool = %call.name, error = %e, "policy recheck on modified input failed");
                    return Ok((call.id.clone(), result));
                }
            }
        } else {
            verdict
        };
        // 合并后 verdict 可能升级为 Deny：重建 dispatch_cfg/is_hard_deny（硬拒绝语义，A-2），
        // 保证 C-21（builtin Deny 不被 Hook Allow 覆盖）对改写后输入同样成立
        let dispatch_cfg = if input_modified {
            self.build_dispatch_config(&verdict)
        } else {
            dispatch_cfg
        };
        let is_hard_deny = matches!(verdict, Verdict::Deny(_));

        // PreToolUse 直出决策与合并 verdict 冲突时取严（Hook Allow 不能越过重查 Deny）
        let pre_decision = match (&pre_decision, &verdict) {
            (Some(Decision::Allow), Verdict::Deny(reason)) => Some(Decision::Deny(format!(
                "输入被 Hook 修改后未通过策略复查: {reason}"
            ))),
            _ => pre_decision,
        };

        // 4. 解析为最终决策（PreToolUse 直出 / Verdict Allow|Deny / Ask→PermissionRequest Hook→prompter）。
        //    Ask 场景传 effective_call——弹窗展示的是实际将执行的（可能被 Hook 改写的）输入。
        let (decision, prompt_id, audit_note) = if let Some(d) = pre_decision {
            (d, None, None)
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
                is_hard_deny,
            )
            .await?
        };

        // 5. 落审计（所有副作用权限决策均落盘，AGENTS.md §5.5；
        //    C-04：detail 不含工具输入原文，避免凭证外泄）
        self.record_permission_audit(&call.name, &decision, prompt_id, audit_note.as_deref())
            .await;

        // Metrics: 记录权限决策
        let verdict_str = match &decision {
            Decision::Allow | Decision::AllowAlways => "allow",
            Decision::Deny(_) | Decision::DenyAlways(_) => "deny",
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
        // AllowAlways/DenyAlways 已在 resolve_decision 持久化并折叠为
        // Allow/Deny——此处仅防御性兜底。
        let result = match decision {
            Decision::AllowAlways | Decision::DenyAlways(_) => {
                unreachable!("AllowAlways/DenyAlways must be collapsed in resolve_decision")
            }
            Decision::Deny(msg) => {
                // 拒绝路径同样发 ToolCallStarted/Finished（2026-08-23 审查 §4-P2）：
                // SSE 消费者视角此前"凭空出现一条错误结果"，无卡片/终态事件。
                let result = ToolResult::err_text(format!("permission denied: {msg}"));
                self.emit_denied_lifecycle(&call.id, &call.name, &result);
                Ok((call.id.clone(), result))
            }
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
        // 命名澄清（A-2，2026-08-25 审查）：当前 BuiltinPolicy 的 `Verdict::Deny`
        // 恰好只产 L0 黑名单拒绝，故"策略 Deny ⇒ 硬拒绝、Hook Allow 不可翻案"
        // 成立。但这是对策略行为的**约定**而非类型保证——Plan 门/路径越界 Deny
        // 也会被并入此通道（方向更严，无害）。若未来引入可被 Hook 覆盖的软
        // Deny，需先给 `Verdict` 增加显式变体区分硬/软，再改此判定。
        let is_hard_deny = matches!(verdict, Verdict::Deny(_));
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
            HookDecision::Allow if !is_hard_deny => {
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
        // inject_context 接线（2026-08-23 审查遗留#6）：缓冲至下一请求 system 段
        //（不能在此 append——会切断 `tool_use`/`tool_result` 配对）
        for ctx_text in &result.inject_contexts {
            self.pending_hook_contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(ctx_text.clone());
        }
        // asyncRewake 接线（遗留#6 全量）：hook 声明后台继续时，经调度器在
        // 后台重新派发同一 hook 收集最终输出（C-26/C-32 由调度器与脚本层保证）。
        // CORE-13（2026-08-25 R2 审查）：契约门控——`PreToolUse` 按协议不支持
        // async_rewake（trait_def `supports_async_rewake`）。ScriptHookRegistry
        // 已过滤，此处再校验是对自定义 `HookRegistry` 实现的纵深防御：防止
        // 未按契约过滤的实现把 PreToolUse 的 rewake 注入本同步路径。
        if let Some(spec) = result.async_rewake.clone()
            && HookEvent::PreToolUse.supports_async_rewake()
        {
            let hook_input = self
                .build_hook_input(HookEvent::PreToolUse, call, side_effect, Some(verdict))
                .await;
            let cfg2 = dispatch_cfg.clone();
            let registry = std::sync::Arc::clone(&self.hook_registry);
            let fut: crate::provider::BoxFuture<'static, Result<String, String>> =
                Box::pin(async move {
                    // 后台重跑同一 hook；输出以 exit_messages 汇总（fatal 已由
                    // dispatch 按 on_error 策略折入 fatal_error 字段）
                    let out = registry.dispatch(hook_input, cfg2).await;
                    if out.fatal_error.is_some() {
                        Err(out
                            .fatal_error
                            .map_or_else(|| "unknown".to_string(), |e| e.to_string()))
                    } else {
                        Ok(out.exit_messages.join("\n"))
                    }
                });
            let accepted = self.rewake.try_spawn(
                &call.name,
                spec.estimated_duration_sec,
                spec.description.clone(),
                fut,
            );
            if !accepted {
                tracing::debug!(tool = %call.name, "asyncRewake rejected by scheduler");
            }
        }
        Ok(pre_decision)
    }

    /// 解析为最终决策（PreToolUse 未直出决策时）。
    ///
    /// - `Allow` / `Deny` → 直出
    /// - `Ask` → 先跑 `PermissionRequest` Hook（可能短路）；未短路则走 `prompter`
    ///
    /// 返回 `(Decision, Option<prompt_id>, Option<audit_note>)`：`prompt_id` 为
    /// `Some` 表示经用户交互；`audit_note` 为 `Some` 时覆盖审计 detail（Always
    /// 决策折叠后仍能区分"持久化@目录"/"会话级"来源，2026-08-25 审查 S-1）。
    #[allow(clippy::too_many_lines)] // Ask 分支含会话缓存/持久化查表/Hook/交互四段决策路径，拆分反而切断因果链
    async fn resolve_decision(
        &self,
        verdict: &Verdict,
        call: &ToolCall,
        side_effect: SideEffect,
        dispatch_cfg: &DispatchConfig,
        is_hard_deny: bool,
    ) -> Result<(Decision, Option<String>, Option<String>), RuntimeError> {
        match verdict {
            Verdict::Allow => Ok((Decision::Allow, None, None)),
            Verdict::Deny(msg) => Ok((Decision::Deny(msg.clone()), None, None)),
            Verdict::Ask(prompt) => {
                // 会话级 Allow 缓存（S-1）：无路径工具的 AllowAlways 落在此处，
                // 本会话内同工具后续调用免弹窗（会话结束即失效，不跨项目）。
                if self
                    .session_allows
                    .lock()
                    .is_ok_and(|s| s.contains(&call.name))
                {
                    tracing::info!(tool = %call.name, "session-scoped allow hit, skipping prompt");
                    return Ok((Decision::Allow, None, None));
                }
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
                    HookDecision::Allow if !is_hard_deny => {
                        // Hook 自动批准，跳过 prompter
                        Ok((Decision::Allow, None, None))
                    }
                    HookDecision::Deny => {
                        let reason = result
                            .reason
                            .unwrap_or_else(|| "blocked by hook".to_string());
                        Ok((Decision::Deny(reason), None, None))
                    }
                    _ => {
                        // fs.* 类工具取 input.path 相对路径（持久化查表与目录
                        // 粒度 Always 持久化共用）。SEC-3（2026-08-25 R2 审查）：
                        // 先做词法规范化——原始输入可能含 `..`/`.` 段，未规范化
                        // 的前缀匹配会把 `src/gen/../secret.txt` 误判进已批准的
                        // `src/gen` 目录范围。
                        let rule_path = call
                            .input
                            .get("path")
                            .and_then(|v| v.as_str())
                            .map(crate::util::normalize_lexical_rel_path);
                        // SEC-1（2026-08-25 R2 审查）：持久化查表与会话级缓存
                        // **同门控**——仅当本 prompt 提供 Always 选项时才可命中。
                        // C-23/C-27 的 restricted ask（options 不含 AllowAlways）
                        // 若被此前对同工具的 Always 批准静默放行，即击穿"不可
                        // Always"通道（如指令性 auto.md 写入的记忆投毒路径）。
                        let always_allowed = prompt
                            .options
                            .contains(&crate::policy::PromptOption::AllowAlways);
                        if always_allowed {
                            // S-1：无路径工具的 AllowAlways 会话级缓存命中免弹窗
                            //（会话结束即失效，不跨项目）。SEC-14：审计注记保留
                            // "session-scoped cache hit" 来源，与首次批准同源可追溯。
                            if self
                                .session_allows
                                .lock()
                                .is_ok_and(|s| s.contains(&call.name))
                            {
                                tracing::info!(
                                    tool = %call.name,
                                    "session-scoped allow hit, skipping prompt"
                                );
                                return Ok((
                                    Decision::Allow,
                                    None,
                                    Some(format!(
                                        "user allowed {tool} always \
                                         (session-scoped cache hit)",
                                        tool = call.name
                                    )),
                                ));
                            }
                            // 遗留#3：持久化规则查表
                            if let Some(decision) =
                                self.lookup_persisted_decision(call, rule_path.as_deref())
                            {
                                return Ok((decision, None, None));
                            }
                        }
                        // Hook 未决策 → 走 prompter 交互
                        let prompt_id = prompt.id.clone();
                        self.events.emit(Event::PermissionRequested {
                            id: prompt.id.clone(),
                            tool: prompt.tool.clone(),
                            summary: prompt.summary.clone(),
                            risk: prompt.risk,
                        });
                        let mut d = self.prompter.prompt(prompt.clone()).await;
                        // 遗留#3：Always 决策持久化后折叠为一次性语义执行。
                        // S-1 粒度收敛（2026-08-25 审查）：带路径工具按**父目录**
                        // 持久化（`tool@目录`，与 decision_for_path 查询对齐）；
                        // 无路径工具只做会话级放行——杜绝"一次按键=跨会话/跨项目
                        // 全局永久放行"。
                        let rule_dir = rule_path.as_ref().and_then(|p| {
                            camino::Utf8Path::new(p.as_str())
                                .parent()
                                .filter(|dir| !dir.as_str().is_empty())
                                .map(std::string::ToString::to_string)
                        });
                        let mut audit_note: Option<String> = None;
                        match &d {
                            Decision::AllowAlways | Decision::DenyAlways(_) => {
                                audit_note = self.persist_and_collapse_always(
                                    call,
                                    &mut d,
                                    rule_dir.as_deref(),
                                );
                            }
                            _ => {}
                        }
                        if audit_note.is_none() {
                            // 无持久化注入：Always 折叠但不落盘（与旧行为一致）
                            d = match d {
                                Decision::AllowAlways => Decision::Allow,
                                Decision::DenyAlways(r) => Decision::Deny(r),
                                other => other,
                            };
                        }
                        let event = Event::PermissionResolved {
                            id: prompt_id.clone(),
                            decision: d.clone(),
                        };
                        self.persist_event(&event).await;
                        self.events.emit(event);
                        Ok((d, Some(prompt_id), audit_note))
                    }
                }
            }
        }
    }

    /// 遗留#3：持久化规则查表（`tool` / `tool@路径前缀`）。
    ///
    /// `rule_path` 为工具输入的相对路径；命中返回折叠后的 `Allow`/`Deny` 决策，
    /// 未注入持久化或无记录返回 `None`。
    fn lookup_persisted_decision(
        &self,
        call: &ToolCall,
        rule_path: Option<&str>,
    ) -> Option<Decision> {
        let store = self.policy_persist.as_ref()?;
        let allow = store.decision_for_path(&call.name, rule_path)?;
        tracing::info!(tool = %call.name, allow, "persisted policy hit, skipping prompt");
        Some(if allow {
            Decision::Allow
        } else {
            Decision::Deny(format!("persisted deny for {}", call.name))
        })
    }

    /// Always 决策持久化并折叠为一次性语义（S-1 粒度收敛，2026-08-25 审查）。
    ///
    /// - `AllowAlways` + 路径 → `tool@父目录` 规则落盘；无路径 → 会话级放行；
    /// - `DenyAlways` + 路径 → `tool@父目录` deny；无路径 → 工具级全局 deny
    ///   （拒绝方向 fail-closed，全局生效是安全的）。
    ///
    /// 返回审计 detail 注记（区分"持久化@目录/会话级/全局 deny"来源）；
    /// `policy_persist` 未注入时返回 `None`（调用方退化为纯折叠）。
    fn persist_and_collapse_always(
        &self,
        call: &ToolCall,
        d: &mut Decision,
        rule_dir: Option<&str>,
    ) -> Option<String> {
        let store = self.policy_persist.as_ref()?;
        match d {
            Decision::AllowAlways => {
                if let Some(dir) = rule_dir {
                    if let Err(e) = store.set_allow_path(&call.name, dir) {
                        tracing::warn!(error = %e, tool = %call.name, "policy.toml 写入失败");
                    }
                    *d = Decision::Allow;
                    Some(format!(
                        "user allowed {tool} always @ {dir} (persisted)",
                        tool = call.name
                    ))
                } else {
                    if let Ok(mut s) = self.session_allows.lock() {
                        s.insert(call.name.clone());
                    }
                    *d = Decision::Allow;
                    Some(format!(
                        "user allowed {tool} always (session-scoped)",
                        tool = call.name
                    ))
                }
            }
            Decision::DenyAlways(reason) => {
                let reason = reason.clone();
                let res = if let Some(dir) = rule_dir {
                    store.set_deny_path(&call.name, dir, &reason)
                } else {
                    store.set_deny(&call.name, &reason)
                };
                if let Err(e) = res {
                    tracing::warn!(error = %e, tool = %call.name, "policy.toml 写入失败");
                }
                *d = Decision::Deny(reason.clone());
                Some(match rule_dir {
                    Some(dir) => format!(
                        "user denied {tool} always @ {dir}: {reason}",
                        tool = call.name
                    ),
                    None => format!("user denied {tool} always: {reason}", tool = call.name),
                })
            }
            _ => None,
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
            // S23：真实轮次（此前恒 0，Hook 脚本拿到的轮次信息无效——
            // 2026-08-23 审查 §10-P2）
            turn: self.current_turn.load(std::sync::atomic::Ordering::Relaxed),
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
    /// （Allow/Deny）。`audit_note` 为 `Some` 时覆盖默认 detail（用于 Always 决策
    /// 折叠后保留"持久化@目录/会话级"来源，2026-08-25 审查 S-1）。审计落盘失败
    /// 仅记 `warn` 日志，不中断工具执行——审计失败不应阻断主流程，但会被运维
    /// 发现并处理。
    pub(crate) async fn record_permission_audit(
        &self,
        tool: &str,
        decision: &Decision,
        prompt_id: Option<String>,
        audit_note: Option<&str>,
    ) {
        let (decision_str, detail) = match (decision, prompt_id.is_some()) {
            (Decision::Allow, true) => ("allow", format!("user allowed {tool}")),
            (Decision::Allow, false) => ("allow", format!("policy allowed {tool}")),
            (Decision::AllowAlways, _) => {
                ("allow", format!("user allowed {tool} always (persisted)"))
            }
            (Decision::DenyAlways(reason), _) => {
                ("deny", format!("user denied {tool} always: {reason}"))
            }
            (Decision::Deny(reason), true) => ("deny", format!("user denied {tool}: {reason}")),
            (Decision::Deny(reason), false) => ("deny", format!("policy denied {tool}: {reason}")),
        };
        // Always 决策在 resolve_decision 已折叠为 Allow/Deny，此处以 note 保留
        // 真实来源（此前 "(persisted)" 分支不可达，审计无法区分一次性与永久授权）
        let detail = audit_note.map_or(detail, ToOwned::to_owned);
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
