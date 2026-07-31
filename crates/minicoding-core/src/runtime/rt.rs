//! `Runtime` 聚合根 + 单轮 Agent 循环（M1 简化版）。
//!
//! M1 简化：
//! - 不接入 `PermissionPolicy`/`PermissionPrompter`（M2）
//! - 不接入 `SandboxDriver`（M4）
//! - 不接入 `HookRegistry`（M5）
//! - 工具执行：无副作用并行、有副作用串行（保留调度框架，权限检查留 M2）
//!
//! 详见 `design.md` §2。

use crate::config::RuntimeConfig;
use crate::context::ContextManager;
use crate::model::{
    Message, RuntimeError, Session, SideEffect, StopReason, ToolCall, ToolCallId, ToolResult,
    TurnOutcome, UserInput,
};
use crate::provider::{ChatRequest, Delta, LlmProvider};
use crate::runtime::accumulator::DeltaAccumulator;
use crate::runtime::event::{Event, EventBus};
use crate::storage::Storage;
use crate::tool::{ToolContext, ToolRegistry};
use camino::Utf8PathBuf;
use futures::StreamExt;
use std::sync::Arc;

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

    /// 返回工作目录。
    #[must_use]
    pub fn workdir(&self) -> &Utf8PathBuf {
        &self.workdir
    }

    /// 驱动单轮对话（用户输入 → 最终回复或失败）。
    ///
    /// 循环不变量（见 `design.md` §2.1）：
    /// 1. 消息先写盘（`storage.append`）再入上下文（`ctx.append`）再广播
    /// 2. 无工具调用时立即 `TurnEnd` 退出
    /// 3. 有工具调用则执行后回到步骤 2
    ///
    /// # Errors
    /// LLM 调用失败、工具执行失败、存储失败等返回 `RuntimeError`。
    pub async fn run_turn(&self, user_input: UserInput) -> Result<TurnOutcome, RuntimeError> {
        let span = tracing::info_span!("turn", session = %self.session.id);
        let _enter = span.enter();

        // 1. 构造用户消息并入库
        let user_msg = Message::user_text(user_input.text);
        self.storage
            .append(&self.session.id, &user_msg)
            .await
            .map_err(RuntimeError::Storage)?;
        self.ctx.append(user_msg.clone()).await;
        self.events.emit(Event::MessageAppended(user_msg));

        let max_iters = self.config.context.max_tool_iters;
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
    }

    /// 流式调用 LLM 并聚合为 assistant 消息。
    async fn stream_llm(&self, req: ChatRequest) -> Result<Message, crate::model::LlmError> {
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

    /// 执行工具调用（无副作用并行、有副作用串行）。
    ///
    /// M1 简化：不接入权限检查（M2）。调度规则保留。
    async fn execute_tool_calls(
        &self,
        calls: &[ToolCall],
    ) -> Result<Vec<(ToolCallId, ToolResult)>, RuntimeError> {
        let ctx = ToolContext::new(self.workdir.clone(), self.session.id.clone());

        // 分桶：无副作用 → 并行；有副作用 → 串行
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

        // 有副作用：严格串行
        for call in &side_effect {
            self.events.emit(Event::ToolCallStarted {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            });
            let result = self.tools.dispatch(call, &ctx).await?;
            self.events.emit(Event::ToolCallFinished {
                call_id: call.id.clone(),
                result: result.clone(),
            });
            results.push((call.id.clone(), result));
        }

        // 按 LLM 原始顺序回填，保证 tool_result 与 tool_calls 一一对应
        results.sort_by_key(|(id, _)| calls.iter().position(|c| c.id == *id).unwrap_or(usize::MAX));

        Ok(results)
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
