//! `ContextManagerImpl`：替代 `SimpleContextManager` 的完整实现（M3 T-M3-1）。
//!
//! 持有消息列表、系统提示词、注入的 `Tokenizer`、`TokenBudget`、可选的
//! `LlmProvider`（供 L2 摘要使用）。在 `build_chat_request` 检查 token 是否超
//! 压缩阈值，超了则调 `compress`（T-M3-2 4 级压缩管道 + T-M3-3 熔断/降级链）。
//! `token_count` 用 `AtomicUsize` 缓存，`append` 增量更新、`restore`/`snapshot`
//! 全量重算。
//!
//! 详见 `docs/design.md` §3、`docs/modules.md` §2。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use minicoding_core::config::RuntimeConfig;
use minicoding_core::context::{ContextManager, ContextSnapshot};
use minicoding_core::model::{Message, RuntimeError};
use minicoding_core::provider::{BoxFuture, ChatRequest, GenerationParams, LlmProvider, Tokenizer};
use minicoding_core::tool::ToolRegistry;
use tokio::sync::Mutex;

use crate::budget::TokenBudget;
use crate::compress::{CircuitBreaker, StateKeep, compress_pipeline};

/// 完整上下文管理器（M3 T-M3-1）。
///
/// 相比 M1 的 `SimpleContextManager`：注入 `Tokenizer` 做精确 token 计数、
/// 持有 `TokenBudget` 控制预算、`compress` 实现 4 级压缩管道（T-M3-2）+ 熔断
/// 状态机（T-M3-3）。`provider` 为 `None` 时跳过 L2 摘要（L1→L3→L4 仍按序执行）。
pub struct ContextManagerImpl {
    messages: tokio::sync::RwLock<Vec<Message>>,
    system_prompt: String,
    tokenizer: Arc<dyn Tokenizer>,
    budget: TokenBudget,
    // L2 摘要所需的 LLM provider；None 时跳过 L2。
    provider: Option<Arc<dyn LlmProvider>>,
    // 同步计数器：`message_count`/`token_count` 是 sync 方法，无法获取 tokio async 锁，
    // 故用 AtomicUsize 在 `append`/`restore` 时同步维护。
    count: AtomicUsize,
    // token 缓存：`append` 增量更新，`restore`/`snapshot` 全量重算。
    token_cache: AtomicUsize,
    // 压缩熔断状态机（C-29：状态机在 Runtime 层，非 LLM 控制，见 §3.6）。
    circuit_breaker: Mutex<CircuitBreaker>,
}

impl ContextManagerImpl {
    /// 创建指定系统提示词、分词器、上下文窗口与可选 provider 的上下文管理器。
    ///
    /// `provider` 为 `Some` 时启用 L2 摘要压缩；为 `None` 时跳过 L2。
    #[must_use]
    pub fn new(
        system_prompt: String,
        tokenizer: Arc<dyn Tokenizer>,
        context_window: usize,
        provider: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        Self {
            messages: tokio::sync::RwLock::new(Vec::new()),
            system_prompt,
            tokenizer,
            budget: TokenBudget::new(context_window),
            provider,
            count: AtomicUsize::new(0),
            token_cache: AtomicUsize::new(0),
            circuit_breaker: Mutex::new(CircuitBreaker::new()),
        }
    }

    /// 创建使用默认系统提示词的上下文管理器。
    ///
    /// `provider` 为 `Some` 时启用 L2 摘要压缩；为 `None` 时跳过 L2。
    #[must_use]
    pub fn with_default_system(
        tokenizer: Arc<dyn Tokenizer>,
        context_window: usize,
        provider: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        Self::new(
            "You are minicoding, a terminal AI coding assistant. Follow the user's instructions carefully.".into(),
            tokenizer,
            context_window,
            provider,
        )
    }

    /// 压缩管道入口（T-M3-2 + T-M3-3 熔断/降级链）。
    ///
    /// 按 `docs/design.md` §3.3 实现 4 级压缩（裁剪→摘要→滚动→硬截断），并集成
    /// §3.6 熔断状态机与 §3.7 状态保留：
    ///
    /// 1. 快照跨压缩状态（`StateKeep`，§3.7）
    /// 2. 调 `compress_pipeline`（含 L2 降级链，§3.8）
    /// 3. 压缩成功且 token ≤ 阈值 → `record_success`
    /// 4. 压缩成功但 token 仍超阈值 → `record_oversize`；`is_thrashing` 时熔断
    /// 5. 压缩失败 → `record_failure`；`should_trip`/`should_force_end` 时熔断
    /// 6. 断言跨压缩状态未被篡改（debug 模式）
    ///
    /// # Errors
    /// - 熔断触发（`should_trip`/`should_force_end`/`is_thrashing`）→ `BudgetExceeded`
    /// - 压缩管道失败（降级链终端也失败，理论不可达）→ 原始 `RuntimeError`
    pub async fn compress(&self) -> Result<(), RuntimeError> {
        let state_keep = StateKeep::snapshot(&self.system_prompt);
        let provider_ref = self.provider.as_deref();

        // 持写锁运行压缩管道，释放后再获取熔断器锁（避免锁序倒置：messages → breaker）。
        let (outcome, new_tokens) = {
            let mut guard = self.messages.write().await;
            let outcome = compress_pipeline(
                &mut guard,
                self.tokenizer.as_ref(),
                &self.budget,
                provider_ref,
            )
            .await;
            // 压缩后重算 token 缓存（messages 可能已变更）
            let tokens = self.tokenizer.count_messages(&guard);
            self.token_cache.store(tokens, Ordering::SeqCst);
            self.count.store(guard.len(), Ordering::SeqCst);
            (outcome, tokens)
        }; // 写锁释放

        let threshold = self.budget.compact_threshold();
        let mut breaker = self.circuit_breaker.lock().await;

        match outcome {
            Ok(_) => {
                if new_tokens > threshold {
                    // 压缩后仍超阈值（Thrash 前兆）
                    breaker.record_oversize();
                    if breaker.is_thrashing() {
                        tracing::warn!(
                            fail_count = breaker.fail_count(),
                            consecutive_oversize = breaker.consecutive_oversize(),
                            "压缩熔断：Thrash 检测触发（连续超阈值），中止本轮"
                        );
                        return Err(RuntimeError::BudgetExceeded {
                            used: new_tokens,
                            budget: threshold,
                        });
                    }
                    tracing::warn!(
                        tokens = new_tokens,
                        threshold,
                        consecutive_oversize = breaker.consecutive_oversize(),
                        "压缩后仍超阈值（未 Thrash，继续发送）"
                    );
                } else {
                    breaker.record_success();
                }
            }
            Err(e) => {
                breaker.record_failure();
                if breaker.should_force_end() {
                    tracing::warn!(
                        fail_count = breaker.fail_count(),
                        "压缩熔断：失败计数 ≥ 5，强制 TurnEnd"
                    );
                    return Err(RuntimeError::BudgetExceeded {
                        used: new_tokens,
                        budget: threshold,
                    });
                }
                if breaker.should_trip() {
                    tracing::warn!(
                        fail_count = breaker.fail_count(),
                        "压缩熔断：失败计数 ≥ 3，中止本轮"
                    );
                    return Err(RuntimeError::BudgetExceeded {
                        used: new_tokens,
                        budget: threshold,
                    });
                }
                // fail_count < 3：传播原始错误（降级链已在 pipeline 内尝试）
                tracing::warn!(
                    fail_count = breaker.fail_count(),
                    "压缩失败但未达熔断阈值，传播错误"
                );
                return Err(e);
            }
        }

        state_keep.assert_unchanged(&self.system_prompt);
        Ok(())
    }
}

impl fmt::Debug for ContextManagerImpl {
    // `Arc<dyn Tokenizer>` 不要求 `Debug`，且 `RwLock` 内消息无法在同步 fmt 中加锁，
    // 故手动实现：展示预算与计数快照，其余用 non_exhaustive。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContextManagerImpl")
            .field("system_prompt", &self.system_prompt)
            .field("budget", &self.budget)
            .field("message_count", &self.count.load(Ordering::SeqCst))
            .field("token_cache", &self.token_cache.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl ContextManager for ContextManagerImpl {
    fn append(&self, msg: Message) -> BoxFuture<'_, ()> {
        // 增量计算新消息 token（含消息框架开销），append 后加到缓存。
        let delta = self.tokenizer.count_messages(std::slice::from_ref(&msg));
        Box::pin(async move {
            self.messages.write().await.push(msg);
            self.count.fetch_add(1, Ordering::SeqCst);
            self.token_cache.fetch_add(delta, Ordering::SeqCst);
        })
    }

    fn build_chat_request(
        &self,
        tools: &ToolRegistry,
        config: &RuntimeConfig,
    ) -> BoxFuture<'_, Result<ChatRequest, RuntimeError>> {
        // 在 async 块外提取所需数据，避免 future 捕获 tools/config 引用
        // （其生命周期与 &self 独立，捕获会导致生命周期不匹配）。
        let tool_schemas = tools.schemas();
        let model = config.provider.model.clone();
        let compress_enabled = config.context.compress;
        Box::pin(async move {
            // 检查是否触发压缩阈值（缓存计数，无需加锁）；超阈值先压缩再读消息，
            // 避免 compress 的写锁与下方读锁死锁（RwLock 不可重入）。
            // compress=off 时跳过压缩直通（C-18 软约束，用户显式关闭）。
            if compress_enabled && self.token_count() > self.budget.compact_threshold() {
                // C-29：熔断状态机在 Runtime 层，压缩前检查是否已熔断。
                // 锁获取后立即检查并释放，不与 messages 锁同时持有（无死锁风险）。
                {
                    let breaker = self.circuit_breaker.lock().await;
                    if breaker.should_trip() || breaker.is_thrashing() {
                        tracing::warn!(
                            fail_count = breaker.fail_count(),
                            consecutive_oversize = breaker.consecutive_oversize(),
                            "压缩熔断已触发，拒绝 build_chat_request"
                        );
                        return Err(RuntimeError::BudgetExceeded {
                            used: self.token_count(),
                            budget: self.budget.compact_threshold(),
                        });
                    }
                } // 熔断器锁释放
                self.compress().await?;
            }
            let guard = self.messages.read().await;
            // ProviderConfig 暂无 temperature/max_output_tokens 字段，M1 置 None。
            let params = GenerationParams {
                model,
                temperature: None,
                top_p: None,
                max_output_tokens: None,
                stop: Vec::new(),
                seed: None,
            };
            Ok(ChatRequest {
                system: self.system_prompt.clone(),
                messages: guard.clone(),
                tools: tool_schemas,
                params,
            })
        })
    }

    fn snapshot(&self) -> BoxFuture<'_, ContextSnapshot> {
        Box::pin(async move {
            // 先 clone 释放读锁，再精确计算 token，避免长持有锁阻塞写者。
            let messages = self.messages.read().await.clone();
            let token_count = self.tokenizer.count_messages(&messages);
            // 同步缓存，保持 token_count() 与快照一致。
            self.token_cache.store(token_count, Ordering::SeqCst);
            ContextSnapshot {
                messages,
                token_count,
            }
        })
    }

    fn restore(&self, snap: ContextSnapshot) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // 全量重算 token 与计数（await 前完成同步借用，避免跨 await 持有 snap 借用）。
            let new_count = snap.messages.len();
            let new_tokens = self.tokenizer.count_messages(&snap.messages);
            let mut guard = self.messages.write().await;
            *guard = snap.messages;
            self.count.store(new_count, Ordering::SeqCst);
            self.token_cache.store(new_tokens, Ordering::SeqCst);
        })
    }

    fn token_count(&self) -> usize {
        self.token_cache.load(Ordering::SeqCst)
    }

    fn message_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}
