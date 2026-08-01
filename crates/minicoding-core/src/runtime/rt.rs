//! `Runtime` 聚合根 + 单轮 Agent 循环。
//!
//! M2 接入：`PermissionPolicy`/`PermissionPrompter`/`AuditSink`——副作用工具调用
//! 必须经权限检查（C-01），决策落 `audit.log`（AGENTS.md §5.5）。
//! 未接入：`SandboxDriver`（M4）、`HookRegistry`（M5）。
//! 工具执行：无副作用并行、有副作用串行（串行段每个工具先过权限）。
//!
//! 详见 `design.md` §2、§9。

use crate::config::RuntimeConfig;
use crate::context::ContextManager;
use crate::journal::Journal;
use crate::memory::SessionSummarizer;
use crate::model::{
    Message, RuntimeError, Session, SideEffect, StopReason, ToolCall, ToolCallId, ToolResult,
    TurnOutcome, UserInput,
};
use crate::policy::{Decision, PermissionContext, PermissionPolicy, PermissionPrompter, Verdict};
use crate::provider::{ChatRequest, Delta, LlmProvider};
use crate::runtime::accumulator::DeltaAccumulator;
use crate::runtime::event::{Event, EventBus};
use crate::sandbox::{
    BreakerState, DenialDetector, SandboxCircuitBreaker, SandboxDriver, SandboxPolicy,
};
use crate::storage::{AuditKind, AuditRecord, AuditSink, Storage};
use crate::tool::{ToolContext, ToolRegistry};
use camino::Utf8PathBuf;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

/// Runtime 聚合根（所有可替换能力的持有者）。
///
/// 由 `RuntimeBuilder` 构造，frontend 长期持有。
/// `run_turn` 驱动单轮 Agent 循环（用户输入 → LLM → 工具 → ... → 最终回复）。
pub struct Runtime {
    pub(crate) provider: Arc<dyn LlmProvider>,
    pub(crate) ctx: Arc<dyn ContextManager>,
    pub(crate) storage: Arc<dyn Storage>,
    pub(crate) tools: ToolRegistry,
    pub(crate) config: RuntimeConfig,
    pub(crate) session: Session,
    pub(crate) events: EventBus,
    pub(crate) workdir: Utf8PathBuf,
    pub(crate) policy: Arc<dyn PermissionPolicy>,
    pub(crate) prompter: Arc<dyn PermissionPrompter>,
    pub(crate) audit: Arc<dyn AuditSink>,
    /// Ctrl-C 取消 token（graceful stop，C-13：已落盘消息不丢失）。
    pub(crate) cancel_token: CancellationToken,
    /// 会话摘要生成器（可选，T-M3-6）。
    ///
    /// `None` 时 `summarize_session` 为 no-op；`Some` 时由 CLI 在会话退出前
    /// 显式调用 `summarize_session`，将摘要落盘到 `index.json` 供跨会话恢复。
    pub(crate) session_summarizer: Option<Arc<dyn SessionSummarizer>>,
    /// OS 沙箱驱动（M4，`shell.run` 在 spawn 子进程前 `apply`，C-22）。
    pub(crate) sandbox_driver: Arc<dyn SandboxDriver>,
    /// OS 沙箱策略（M4，与 `sandbox_driver` 配套）。
    pub(crate) sandbox_policy: SandboxPolicy,
    /// 文件改动 journal（M4，可选，`fs.write/edit/delete` 成功后 `record`，C-28）。
    pub(crate) journal: Option<Arc<dyn Journal>>,
    /// 沙箱拒绝检测器（无状态，T-M4-5）。
    pub(crate) denial_detector: DenialDetector,
    /// 沙箱拒绝熔断器（单 turn 内有效，C-30 不可被 LLM 绕过）。
    pub(crate) sandbox_breaker: SandboxCircuitBreaker,
}

impl Runtime {
    /// 返回当前会话。
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 返回事件总线引用（订阅事件流）。
    #[must_use]
    pub fn events(&self) -> &EventBus {
        &self.events
    }

    /// 返回上下文管理器引用（供 frontend/test 查询 `message_count` 等）。
    #[must_use]
    pub fn context(&self) -> &Arc<dyn ContextManager> {
        &self.ctx
    }

    /// 返回存储引用（供 frontend/test 查询会话消息）。
    #[must_use]
    pub fn storage(&self) -> &Arc<dyn Storage> {
        &self.storage
    }

    /// 返回工作目录。
    #[must_use]
    pub fn workdir(&self) -> &Utf8PathBuf {
        &self.workdir
    }

    /// 返回取消 token 的克隆（供 frontend 在 `select!` 中组合等待，如 Ctrl-C handler）。
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// 触发取消（CLI 的 Ctrl-C handler 调用）。
    ///
    /// 取消是 graceful 的：当前 in-flight 的迭代被丢弃，已落盘的消息保留
    /// （C-13：Ctrl-C 不丢已生成消息），`run_turn` 返回 `TurnOutcome::Interrupted`。
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// 恢复会话历史到上下文管理器（`--resume`/`--fork-session` 用，T-M3-10a）。
    ///
    /// 将 `self.session.messages` 逐条注入 `ContextManager`，使后续 `run_turn` 能
    /// 基于历史上下文继续对话。仅在 `RuntimeBuilder::session` 设置预加载会话后调用
    /// 一次；对新建会话（空消息）调用是 no-op。
    ///
    /// 消息已在磁盘（首次 `storage.append` 时落盘），此处只回填内存上下文，
    /// **不重复落盘**——后续 `run_turn` 的新消息才走 `storage.append`。
    ///
    /// # Errors
    /// 当前 `ContextManager::append` 不返回错误；保留 `Result` 为未来扩展（如
    /// 压缩管道在回填时触发熔断）预留。
    pub async fn restore_history(&self) -> Result<(), RuntimeError> {
        let count = self.session.messages.len();
        for msg in &self.session.messages {
            self.ctx.append(msg.clone()).await;
        }
        if count > 0 {
            tracing::info!(session = %self.session.id, restored = count, "history restored");
        }
        Ok(())
    }

    /// 生成会话摘要并落盘 `index.json`（T-M3-6）。
    ///
    /// 在会话退出前调用：从 `ContextManager` 快照消息 → 调注入的
    /// `SessionSummarizer` 生成摘要（降级链：主 provider → 备用 → 启发式兜底，
    /// C-29 永不失败）→ `Storage::update_summary` 落盘。
    ///
    /// `session_summarizer` 未注入或会话无消息时为 no-op。摘要失败仅记 `warn`
    /// 日志，不阻塞会话退出（best effort，与会话生命周期解耦）。
    ///
    /// # Errors
    /// 仅当 `Storage::update_summary` 失败时返回 `RuntimeError::Storage`；
    /// 摘要生成本身永不失败（启发式兜底，C-29）。
    pub async fn summarize_session(&self) -> Result<(), RuntimeError> {
        let Some(summarizer) = &self.session_summarizer else {
            return Ok(());
        };
        let snap = self.ctx.snapshot().await;
        if snap.messages.is_empty() {
            return Ok(());
        }
        let summary = match summarizer.summarize(&snap.messages).await {
            Ok(s) => s,
            Err(e) => {
                // 理论不可达：启发式兜底恒成功（C-29）。但保留兜底以防实现 bug。
                tracing::warn!(
                    error = %e,
                    session = %self.session.id,
                    "会话摘要生成失败（理论不可达，C-29 兜底应保证成功）"
                );
                return Ok(());
            }
        };
        if let Err(e) = self
            .storage
            .update_summary(&self.session.id, &summary)
            .await
        {
            tracing::warn!(
                error = %e,
                session = %self.session.id,
                "会话摘要落盘失败（best effort，不阻塞退出）"
            );
            return Err(RuntimeError::Storage(e));
        }
        tracing::info!(
            session = %self.session.id,
            summary_chars = summary.chars().count(),
            "会话摘要已落盘"
        );
        Ok(())
    }

    /// 驱动单轮对话（用户输入 → 最终回复或失败）。
    ///
    /// 循环不变量（见 `design.md` §2.1）：
    /// 1. 消息先写盘（`storage.append`）再入上下文（`ctx.append`）再广播
    /// 2. 无工具调用时立即 `TurnEnd` 退出
    /// 3. 有工具调用则执行后回到步骤 2
    ///
    /// 三道终止防御（C-13 单轮调用上限的补充）：
    /// - `max_tool_iters`：迭代轮次硬上限（默认 50）
    /// - 重复检测：连续 ≥3 轮相同工具调用集合 → 判定死循环提前终止
    /// - `turn_timeout`：整个 turn 超时（默认 600s）→ `Stopped`
    /// - Ctrl-C cancel：`cancel()` 触发 → `Interrupted`（已落盘消息不丢失）
    /// - 沙箱拒绝熔断（C-30）：单 turn 内 ≥3 次拒绝注入提醒，≥5 次强制 `TurnEnd`
    ///
    /// # Errors
    /// LLM 调用失败、工具执行失败、存储失败等返回 `RuntimeError`。
    pub async fn run_turn(&self, user_input: UserInput) -> Result<TurnOutcome, RuntimeError> {
        let span = tracing::info_span!("turn", session = %self.session.id);
        let _enter = span.enter();

        // turn 开始：重置沙箱拒绝熔断器（单 turn 内有效，C-30）
        self.sandbox_breaker.reset();

        // 1. 构造用户消息并入库
        let user_msg = Message::user_text(user_input.text);
        self.storage
            .append(&self.session.id, &user_msg)
            .await
            .map_err(RuntimeError::Storage)?;
        self.ctx.append(user_msg.clone()).await;
        self.events.emit(Event::MessageAppended(user_msg));

        let max_iters = self.config.context.max_tool_iters;
        let turn_timeout = Duration::from_secs(self.config.context.turn_timeout_sec);

        // 主循环封装为 future，由外层 select! 与 timeout/cancel 组合
        let turn_fut = async {
            // 重复检测：记录每轮工具调用签名，连续 3 轮相同 → 死循环
            let mut call_signatures: Vec<String> = Vec::new();

            for _iter in 0..max_iters {
                // 2. 构建请求（system + tools + 压缩后的历史）
                let req = self
                    .ctx
                    .build_chat_request(&self.tools, &self.config)
                    .await?;

                // 3. 流式调用 LLM
                let assistant_msg = match self.stream_llm(req).await {
                    Ok(msg) => msg,
                    Err(e) => return Ok(TurnOutcome::Failed(e.into())),
                };

                // 4. 落盘 assistant 消息
                self.storage
                    .append(&self.session.id, &assistant_msg)
                    .await
                    .map_err(RuntimeError::Storage)?;
                self.ctx.append(assistant_msg.clone()).await;
                self.events
                    .emit(Event::MessageAppended(assistant_msg.clone()));

                // 5. 无工具调用 → 终止
                if assistant_msg.tool_calls.is_empty() {
                    self.events.emit(Event::TurnEnd {
                        stop_reason: StopReason::EndTurn,
                    });
                    return Ok(TurnOutcome::Finished(assistant_msg));
                }

                // 5.1 重复检测：连续 ≥3 轮相同工具调用集合 → 死循环，提前终止
                //     （C-13 补充：max_tool_iters 之外的早期止损，避免无谓消耗）
                let sig = Self::tool_calls_signature(&assistant_msg.tool_calls);
                call_signatures.push(sig);
                if Self::is_repeating(&call_signatures) {
                    tracing::warn!("turn terminated: repeated tool calls detected");
                    self.events.emit(Event::TurnEnd {
                        stop_reason: StopReason::Stopped,
                    });
                    return Ok(TurnOutcome::Finished(Message::assistant_text(
                        "[检测到重复工具调用，已终止以避免死循环]".to_string(),
                    )));
                }

                // 6. 执行工具调用
                let results = match self.execute_tool_calls(&assistant_msg.tool_calls).await {
                    Ok(r) => r,
                    Err(e) => return Ok(TurnOutcome::Failed(e)),
                };

                // 7. 落盘 tool_result 并入上下文
                for (id, result) in &results {
                    let msg = Self::tool_result_message(id.clone(), result.clone());
                    self.storage
                        .append(&self.session.id, &msg)
                        .await
                        .map_err(RuntimeError::Storage)?;
                    self.ctx.append(msg.clone()).await;
                    self.events.emit(Event::MessageAppended(msg));
                }
            }

            // 达到 max_iters 上限
            tracing::warn!(max_iters, "turn exceeded max tool iterations");
            self.events.emit(Event::TurnEnd {
                stop_reason: StopReason::Stopped,
            });
            Ok(TurnOutcome::Finished(Message::assistant_text(
                "[达到最大工具调用轮次上限]".to_string(),
            )))
        };

        // turn_timeout + Ctrl-C cancel（graceful stop；已落盘消息不丢失，C-13）
        // 三路 select：cancel 优先返回 Interrupted；timeout 返回 Finished(Stopped)；
        // turn_fut 正常完成则透传其 outcome（内部已 emit TurnEnd）。
        tokio::select! {
            () = self.cancel_token.cancelled() => {
                tracing::info!("turn cancelled by user");
                self.events.emit(Event::TurnEnd {
                    stop_reason: StopReason::Interrupted,
                });
                Ok(TurnOutcome::Interrupted(Message::assistant_text(
                    "[已取消]".to_string(),
                )))
            }
            () = tokio::time::sleep(turn_timeout) => {
                tracing::warn!(
                    timeout_sec = self.config.context.turn_timeout_sec,
                    "turn timed out"
                );
                self.events.emit(Event::TurnEnd {
                    stop_reason: StopReason::Stopped,
                });
                Ok(TurnOutcome::Finished(Message::assistant_text(
                    "[turn 超时终止]".to_string(),
                )))
            }
            outcome = turn_fut => outcome,
        }
    }

    /// 计算一轮工具调用的签名（`name|规范化 input`，多调用排序后拼接）。
    ///
    /// `serde_json` 默认对 `Value::Object` 用 `BTreeMap`（键排序），保证 input
    /// 序列化与键顺序无关，跨轮比较稳定。用于重复检测识别"连续相同工具调用集合"。
    fn tool_calls_signature(calls: &[ToolCall]) -> String {
        let mut sigs: Vec<String> = calls
            .iter()
            .map(|c| {
                let input = serde_json::to_string(&c.input).unwrap_or_else(|_| c.input.to_string());
                format!("{}|{}", c.name, input)
            })
            .collect();
        sigs.sort_unstable();
        sigs.join(";")
    }

    /// 检测最近 3 轮工具调用签名是否完全相同（连续 ≥3 轮 → 死循环）。
    fn is_repeating(signatures: &[String]) -> bool {
        let n = signatures.len();
        if n < 3 {
            return false;
        }
        let last = &signatures[n - 1];
        signatures[n - 3..].iter().all(|s| s == last)
    }

    /// 流式调用 LLM 并聚合为 assistant 消息。
    ///
    /// `OTel`：`llm_call` span 包裹整次 provider 调用（design.md §15.1），字段不含
    /// 凭证（C-04：仅记 model 与消息数，不记 input 原文）。
    async fn stream_llm(&self, req: ChatRequest) -> Result<Message, crate::model::LlmError> {
        let span = tracing::info_span!(
            "llm_call",
            model = %req.params.model,
            message_count = req.messages.len(),
            otel.name = "llm_call",
        );
        let _enter = span.enter();

        let mut stream = self.provider.chat_stream(req).await?;
        let mut acc = DeltaAccumulator::new();
        self.events.emit(Event::TurnStreamingStarted);

        while let Some(delta) = stream.next().await {
            let delta = delta?;
            if let Delta::Text(ref s) = delta {
                self.events.emit(Event::Token(s.clone()));
            }
            acc.push(delta);
        }

        Ok(acc.finalize())
    }

    /// 执行工具调用（无副作用并行、有副作用串行 + 权限检查）。
    ///
    /// 副作用工具（`SideEffect != None`）在 dispatch 前必须经
    /// `PermissionPolicy::check` → 必要时 `PermissionPrompter::prompt` → 决策落
    /// `AuditSink`（C-01、AGENTS.md §5.5）。只读工具直接并行执行（BuiltinPolicy
    /// 对 `SideEffect::None` 返回 `Allow`，此处跳过以避免无谓 IO）。
    async fn execute_tool_calls(
        &self,
        calls: &[ToolCall],
    ) -> Result<Vec<(ToolCallId, ToolResult)>, RuntimeError> {
        // 构造 ToolContext：注入沙箱驱动/策略/journal（M4，shell.run/fs 用）
        let ctx = ToolContext::new(self.workdir.clone(), self.session.id.clone())
            .with_sandbox(self.sandbox_driver.clone(), self.sandbox_policy.clone())
            .with_journal_opt(self.journal.clone());

        // 分桶：无副作用 → 并行；有副作用 → 串行（含权限检查）
        let (readonly, side_effect): (Vec<&ToolCall>, Vec<&ToolCall>) =
            calls.iter().partition(|c| {
                self.tools
                    .get(&c.name)
                    .is_none_or(|t| t.side_effect() == SideEffect::None)
            });

        let mut results: Vec<(ToolCallId, ToolResult)> = Vec::with_capacity(calls.len());

        // 无副作用：并发执行（最多 8 并发）
        let ro_futs = readonly.iter().map(|call| {
            let ctx = ctx.clone();
            let call_id = call.id.clone();
            let tool_name = call.name.clone();
            async move {
                // `tool_call` span（design.md §15.1）：只读桶并行执行，每个调用独立 span。
                let span = tracing::debug_span!(
                    "tool_call",
                    tool = %tool_name,
                    call_id = %call_id,
                );
                let _enter = span.enter();
                self.events.emit(Event::ToolCallStarted {
                    call_id: call_id.clone(),
                    tool: tool_name,
                });
                let result = self.tools.dispatch(call, &ctx).await?;
                self.events.emit(Event::ToolCallFinished {
                    call_id: call_id.clone(),
                    result: result.clone(),
                });
                Ok::<_, RuntimeError>((call.id.clone(), result))
            }
        });
        let mut ro_stream = futures::stream::iter(ro_futs).buffer_unordered(8);
        while let Some(r) = ro_stream.next().await {
            results.push(r?);
        }

        // 有副作用：严格串行，每个工具先过权限（见 execute_side_effect_call）
        for call in &side_effect {
            // `tool_call` span（design.md §15.1）：副作用桶串行执行，包裹权限检查 + dispatch。
            let span = tracing::debug_span!(
                "tool_call",
                tool = %call.name,
                call_id = %call.id,
            );
            let _enter = span.enter();
            results.push(self.execute_side_effect_call(call, &ctx).await?);
        }

        // 按 LLM 原始顺序回填，保证 tool_result 与 tool_calls 一一对应
        results.sort_by_key(|(id, _)| calls.iter().position(|c| c.id == *id).unwrap_or(usize::MAX));

        Ok(results)
    }

    /// 对单个副作用工具调用执行权限检查 + 调度（C-01 实现层强制）。
    ///
    /// 流程：策略判定 → 必要时交互 → 落审计 → 按决策执行或拒绝。
    /// `Deny`（策略或用户）返回 `Ok` 带 `is_error=true` 的结果；仅 `dispatch` 失败
    /// 返回 `Err`（与原 `?` 传播语义一致）。
    async fn execute_side_effect_call(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
    ) -> Result<(ToolCallId, ToolResult), RuntimeError> {
        let side_effect = self
            .tools
            .get(&call.name)
            .map_or(SideEffect::None, |t| t.side_effect());

        // `permission` span（design.md §15.1）：包裹权限决策流程（策略判定 →
        // prompter 交互 → 审计落盘）。字段不含 input 原文（C-04）。
        let span = tracing::info_span!(
            "permission",
            tool = %call.name,
            side_effect = ?side_effect,
            otel.name = "permission",
        );
        let _enter = span.enter();

        let perm_ctx = PermissionContext {
            session: self.session.id.clone(),
            workdir: self.workdir.clone(),
            side_effect,
            turn: 0,
            history: Vec::new(),
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

        // 2. 解析为最终决策：Allow/Deny 直出，Ask 走 prompter 点对点交互
        let (decision, prompt_id) = match verdict {
            Verdict::Allow => (Decision::Allow, None),
            Verdict::Deny(msg) => (Decision::Deny(msg), None),
            Verdict::Ask(prompt) => {
                let prompt_id = prompt.id.clone();
                self.events.emit(Event::PermissionRequested {
                    id: prompt.id.clone(),
                    tool: prompt.tool.clone(),
                    summary: prompt.summary.clone(),
                    risk: prompt.risk,
                });
                let d = self.prompter.prompt(prompt).await;
                self.events.emit(Event::PermissionResolved {
                    id: prompt_id.clone(),
                    decision: d.clone(),
                });
                (d, Some(prompt_id))
            }
        };

        // 3. 落审计（所有副作用权限决策均落盘，AGENTS.md §5.5；
        //    C-04：detail 不含工具输入原文，避免凭证外泄）
        self.record_permission_audit(&call.name, &decision, prompt_id)
            .await;

        // 4. 按决策执行或拒绝
        match decision {
            Decision::Deny(msg) => Ok((
                call.id.clone(),
                ToolResult::err_text(format!("permission denied: {msg}")),
            )),
            Decision::Allow => {
                self.events.emit(Event::ToolCallStarted {
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                });
                let result = match self.tools.dispatch(call, ctx).await {
                    Ok(r) => r,
                    Err(e) => {
                        // 沙箱拒绝检测（T-M4-5）：识别 EPERM/EACCES/landlock 等
                        // 内核级硬反馈，更新熔断器（C-30 不可被 LLM 绕过）。
                        if let Some(denial_result) =
                            self.handle_sandbox_denial(&call.id, &call.name, &e)
                        {
                            return Ok(denial_result);
                        }
                        // 非 denial 错误：原样传播
                        return Err(RuntimeError::Tool {
                            tool: call.name.clone(),
                            source: e,
                        });
                    }
                };
                self.events.emit(Event::ToolCallFinished {
                    call_id: call.id.clone(),
                    result: result.clone(),
                });
                Ok((call.id.clone(), result))
            }
        }
    }

    /// 沙箱拒绝检测与熔断处理（T-M4-5）。
    ///
    /// 检测工具错误是否为沙箱拒绝（EPERM/EACCES/landlock 等）。若是：
    /// - 更新熔断器计数；
    /// - 软熔断（≥3 次）：附加方向提醒返回；
    /// - 硬熔断（≥5 次）：返回带总结的错误；
    /// - 未熔断：返回带 denial 标识的错误，提示 LLM/用户。
    ///
    /// 返回 `Some(ToolResult)` 表示已识别为 denial 并生成回灌结果；
    /// 返回 `None` 表示非 denial，调用方原样传播错误。
    fn handle_sandbox_denial(
        &self,
        call_id: &ToolCallId,
        tool: &str,
        error: &crate::model::ToolError,
    ) -> Option<(ToolCallId, ToolResult)> {
        let error_text = error.to_string();
        let m = self.denial_detector.detect(tool, &error_text)?;
        tracing::warn!(
            tool = %m.tool,
            reason = m.signature.reason,
            platform = m.signature.platform,
            "sandbox denial detected"
        );
        let state = self.sandbox_breaker.record_denial();
        let result = match state {
            BreakerState::HardTripped => {
                let summary = crate::sandbox::hard_trip_summary(self.sandbox_breaker.count());
                tracing::warn!(
                    count = self.sandbox_breaker.count(),
                    "sandbox circuit breaker hard-tripped"
                );
                ToolResult {
                    content: crate::model::ToolContent::Text(format!(
                        "{summary}\n原始错误：{error_text}"
                    )),
                    is_error: true,
                    metadata: crate::model::ToolResultMeta::default(),
                }
            }
            BreakerState::SoftTripped => {
                let reminder = crate::sandbox::soft_trip_reminder(self.sandbox_breaker.count());
                tracing::warn!(
                    count = self.sandbox_breaker.count(),
                    "sandbox circuit breaker soft-tripped"
                );
                ToolResult {
                    content: crate::model::ToolContent::Text(format!(
                        "沙箱拒绝（{reason}）：{error_text}\n\n{reminder}",
                        reason = m.signature.reason
                    )),
                    is_error: true,
                    metadata: crate::model::ToolResultMeta::default(),
                }
            }
            BreakerState::Closed => ToolResult::err_text(format!(
                "sandbox denied ({reason}): {error_text}\n\
                 提示：可切换更宽松的沙箱预设（如 --sandbox workspace-write）重试",
                reason = m.signature.reason
            )),
        };
        Some((call_id.clone(), result))
    }

    /// 记录权限决策审计（C-01 决策可追溯，AGENTS.md §5.5）。
    ///
    /// `prompt_id` 为 `Some` 表示经用户交互（Ask→prompter），`None` 表示策略直出
    /// （Allow/Deny）。审计落盘失败仅记 `warn` 日志，不中断工具执行——审计失败不应
    /// 阻断主流程，但会被运维发现并处理。
    async fn record_permission_audit(
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
        let rec = AuditRecord {
            ts: OffsetDateTime::now_utc(),
            session: self.session.id.clone(),
            kind: AuditKind::PermissionResolved,
            tool: Some(tool.to_string()),
            decision: Some(decision_str.to_string()),
            detail,
        };
        if let Err(e) = self.audit.record(rec).await {
            tracing::warn!(error = %e, "audit record failed");
        }
    }

    /// 构造 `tool_result` 消息。
    fn tool_result_message(call_id: ToolCallId, result: ToolResult) -> Message {
        use crate::model::{ContentBlock, MessageMeta, MessageSource};
        let content = vec![ContentBlock::ToolResult {
            call_id,
            content: result.content,
            is_error: result.is_error,
        }];
        Message {
            id: ulid::Ulid::new().to_string(),
            role: crate::model::Role::Tool,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: time::OffsetDateTime::now_utc(),
            metadata: MessageMeta {
                source: MessageSource::Tool,
                ..Default::default()
            },
        }
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("session_id", &self.session.id)
            .field("workdir", &self.workdir)
            .field("tools_count", &self.tools.len())
            .finish_non_exhaustive()
    }
}
