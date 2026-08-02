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
use minicoding_core::prompt::{PromptContext, PromptPipeline};
use minicoding_core::provider::{BoxFuture, ChatRequest, GenerationParams, LlmProvider, Tokenizer};
use minicoding_core::tool::ToolRegistry;
use tokio::sync::Mutex;

use crate::budget::TokenBudget;
use crate::compress::{
    CircuitBreaker, PostCompactConfig, PredictiveTracker, StateKeep, compress_pipeline,
    extract_read_files, inject_post_compact, should_predict_compact,
};

/// 完整上下文管理器（M3 T-M3-1）。
///
/// 相比 M1 的 `SimpleContextManager`：注入 `Tokenizer` 做精确 token 计数、
/// 持有 `TokenBudget` 控制预算、`compress` 实现 4 级压缩管道（T-M3-2）+ 熔断
/// 状态机（T-M3-3）。`provider` 为 `None` 时跳过 L2 摘要（L1→L3→L4 仍按序执行）。
pub struct ContextManagerImpl {
    messages: tokio::sync::RwLock<Vec<Message>>,
    system_prompt: String,
    /// 可选 Prompt 管道（注入后 `build_chat_request` 动态构建 system prompt，
    /// 覆盖 `system_prompt` 静态字段；未注入时回退到 `system_prompt`）。
    ///
    /// 设计：`PromptPipeline` 与 `PromptContext` 定义在 `minicoding-core`，
    /// `ContextManagerImpl` 直接持有，无需引入 `extension-sdk` 依赖。
    /// 9 个内置 contributor 由 CLI 组装后注入（`minicoding-extension-sdk::builtin_contributors`）。
    pipeline: Option<Arc<PromptPipeline>>,
    /// `PromptContext` 模板（pipeline 启用时使用）。
    ///
    /// `enabled_tools` 字段在 `build_chat_request` 时由 `tools.schemas()` 填充，
    /// 其余字段（`session_id`/`workdir`/`platform`/`git_info`/`user_rules`/`project_rules`）
    /// 在构造时由 CLI 注入（来自 `~/.minicoding/long_term.md` + AGENTS.md 加载结果）。
    prompt_ctx_template: Option<PromptContext>,
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
    // C-08：预测性压缩追踪器（记录每 turn token 历史）。
    predictive_tracker: Mutex<PredictiveTracker>,
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
            pipeline: None,
            prompt_ctx_template: None,
            tokenizer,
            budget: TokenBudget::new(context_window),
            provider,
            count: AtomicUsize::new(0),
            token_cache: AtomicUsize::new(0),
            circuit_breaker: Mutex::new(CircuitBreaker::new()),
            predictive_tracker: Mutex::new(PredictiveTracker::new()),
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

    /// 链式注入 Prompt 管道与 context 模板（启用动态 system prompt 构建）。
    ///
    /// 注入后 `build_chat_request` 优先调 `pipeline.build(&ctx)` 生成 system prompt，
    /// 跳过静态 `system_prompt` 字段。`enabled_tools` 字段在每次 `build_chat_request`
    /// 时由 `tools.schemas()` 动态填充（template 中的值被覆盖）。
    ///
    /// `system_prompt` 仍保留作为 pipeline 失败时的兜底（理论不可达，contributor
    /// 内部应自兜底；保留以防 contributor 实现异常导致启动失败）。
    #[must_use]
    pub fn with_prompt_pipeline(
        mut self,
        pipeline: Arc<PromptPipeline>,
        prompt_ctx_template: PromptContext,
    ) -> Self {
        self.pipeline = Some(pipeline);
        self.prompt_ctx_template = Some(prompt_ctx_template);
        self
    }

    /// 构建基础 system prompt（pipeline 启用时动态构建，否则回退静态字段）。
    ///
    /// `enabled_tools` 由调用方传入（来自 `tools.schemas()`），覆盖 template 中的值。
    /// pipeline 启用但 template 缺失时，回退到静态 `system_prompt`（防御性兜底，
    /// 理论不可达——`with_prompt_pipeline` 同时设置两者）。
    ///
    /// # Errors
    /// pipeline `build` 失败时返回 `RuntimeError::Prompt`。
    async fn build_base_system_prompt(
        &self,
        enabled_tools: &[minicoding_core::model::ToolSchema],
    ) -> Result<String, RuntimeError> {
        match (&self.pipeline, &self.prompt_ctx_template) {
            (Some(pipeline), Some(template)) => {
                let mut ctx = template.clone();
                ctx.enabled_tools = enabled_tools.to_vec();
                let built = pipeline.build(&ctx).await?;
                Ok(built.text)
            }
            _ => Ok(self.system_prompt.clone()),
        }
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
    pub async fn compress(&self, backup_before_compress: bool) -> Result<(), RuntimeError> {
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
                backup_before_compress,
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
        let backup_before_compress = config.context.backup_before_compress;
        let predictive_enabled = config.context.predictive_compact_enabled;
        let predictive_baseline = config.context.predictive_baseline_growth_tokens;
        let post_compact_cfg = PostCompactConfig {
            max_files: config.context.post_compact_max_files,
            token_budget: config.context.post_compact_token_budget,
            max_tokens_per_file: config.context.post_compact_max_tokens_per_file,
        };
        Box::pin(async move {
            let threshold = self.budget.compact_threshold();
            let current_tokens = self.token_count();

            // C-08：预测性压缩——当前未超阈值但预测下一 turn 会超时提前压缩
            let need_predictive = predictive_enabled && current_tokens <= threshold && {
                let tracker = self.predictive_tracker.lock().await;
                should_predict_compact(current_tokens, threshold, &tracker, predictive_baseline)
            };

            // 检查是否触发压缩阈值（缓存计数，无需加锁）；超阈值先压缩再读消息，
            // 避免 compress 的写锁与下方读锁死锁（RwLock 不可重入）。
            // compress=off 时跳过压缩直通（C-18 软约束，用户显式关闭）。
            let did_compress =
                if compress_enabled && (current_tokens > threshold || need_predictive) {
                    // C-29：熔断状态机在 Runtime 层，压缩前检查是否已熔断。
                    {
                        let breaker = self.circuit_breaker.lock().await;
                        if breaker.should_trip() || breaker.is_thrashing() {
                            tracing::warn!(
                                fail_count = breaker.fail_count(),
                                consecutive_oversize = breaker.consecutive_oversize(),
                                "压缩熔断已触发，拒绝 build_chat_request"
                            );
                            return Err(RuntimeError::BudgetExceeded {
                                used: current_tokens,
                                budget: threshold,
                            });
                        }
                    } // 熔断器锁释放
                    self.compress(backup_before_compress).await?;
                    true
                } else {
                    false
                };

            // 构建基础 system prompt：pipeline 启用时动态构建，否则用静态字段。
            // post-compact 注入在 base 之上叠加（无论 pipeline/static 都适用）。
            let base_system = self.build_base_system_prompt(&tool_schemas).await?;

            // C-09：post-compact 上下文恢复——压缩后重新注入最近读过的文件
            let system = if did_compress {
                let guard = self.messages.read().await;
                let read_files = extract_read_files(&guard, post_compact_cfg.max_files);
                drop(guard);
                if read_files.is_empty() {
                    base_system
                } else {
                    let workdir =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    inject_post_compact(
                        &base_system,
                        &read_files,
                        &post_compact_cfg,
                        self.tokenizer.as_ref(),
                        &workdir,
                    )
                }
            } else {
                base_system
            };

            // C-08：记录本 turn token 快照供下次预测
            if predictive_enabled {
                let mut tracker = self.predictive_tracker.lock().await;
                tracker.record_turn(self.token_count());
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
                system,
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

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::config::RuntimeConfig;
    use minicoding_core::context::ContextManager;
    use minicoding_core::model::{Message, SideEffect, ToolError, ToolResult, ToolSchema};
    use minicoding_core::provider::Tokenizer;
    use minicoding_core::tool::{Tool, ToolContext, ToolRegistry};
    use std::sync::Arc;

    use crate::budget::TokenBudget;

    /// 固定 10 token/消息 的分词器（用于简单计数测试）。
    struct TenTokensTokenizer;

    impl Tokenizer for TenTokensTokenizer {
        fn count(&self, text: &str) -> usize {
            text.chars().count().max(10)
        }
        fn count_messages(&self, msgs: &[Message]) -> usize {
            // 每条消息固定 10 token，便于精确断言
            msgs.len() * 10
        }
        fn id(&self) -> &'static str {
            "ten-tokens-test"
        }
    }

    /// 按字符数计数的分词器（1 字符 = 1 token，用于压缩测试）。
    struct CharTokenizer;

    impl Tokenizer for CharTokenizer {
        fn count(&self, text: &str) -> usize {
            text.chars().count()
        }
        fn count_messages(&self, msgs: &[Message]) -> usize {
            msgs.iter().map(|m| m.text().chars().count()).sum()
        }
        fn id(&self) -> &'static str {
            "char-test"
        }
    }

    /// mock 工具：返回固定 schema，execute 不被调用。
    struct MockTool {
        schema: ToolSchema,
    }

    impl MockTool {
        fn new(name: &str, description: &str) -> Self {
            Self {
                schema: ToolSchema {
                    name: name.to_string(),
                    description: description.to_string(),
                    input_schema: serde_json::json!({"type": "object", "properties": {}}),
                },
            }
        }
    }

    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.schema.name
        }
        fn schema(&self) -> &ToolSchema {
            &self.schema
        }
        fn side_effect(&self) -> SideEffect {
            SideEffect::None
        }
        fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext,
        ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
            // 测试中不会调用 execute
            Box::pin(async { Err(ToolError::NotFound("mock tool".into())) })
        }
    }

    // === 场景 1：new/with_default_system 创建实例，初始计数为 0 ===

    #[test]
    fn new_creates_empty_manager() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("system prompt".into(), tokenizer, 10_000, None);
        assert_eq!(mgr.message_count(), 0);
        assert_eq!(mgr.token_count(), 0);
    }

    #[test]
    fn with_default_system_creates_empty_manager() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::with_default_system(tokenizer, 10_000, None);
        assert_eq!(mgr.message_count(), 0);
        assert_eq!(mgr.token_count(), 0);
    }

    // === 场景 2：append 单条消息后计数递增 ===

    #[tokio::test]
    async fn append_single_message_increments_counts() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 10_000, None);
        mgr.append(Message::user_text("hello")).await;
        assert_eq!(mgr.message_count(), 1);
        assert_eq!(mgr.token_count(), 10); // TenTokensTokenizer: 1 条 × 10
    }

    // === 场景 3：append 多条消息后计数累加 ===

    #[tokio::test]
    async fn append_multiple_messages_accumulates_counts() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 10_000, None);
        for i in 0..5u32 {
            mgr.append(Message::user_text(format!("msg {i}"))).await;
        }
        assert_eq!(mgr.message_count(), 5);
        assert_eq!(mgr.token_count(), 50); // 5 条 × 10
    }

    // === 场景 4：snapshot 返回当前消息列表和 token 计数 ===

    #[tokio::test]
    async fn snapshot_returns_messages_and_token_count() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 10_000, None);
        mgr.append(Message::user_text("first")).await;
        mgr.append(Message::user_text("second")).await;
        let snap = mgr.snapshot().await;
        assert_eq!(snap.messages.len(), 2);
        assert_eq!(snap.token_count, 20); // 2 条 × 10
        assert_eq!(snap.messages[0].text(), "first");
        assert_eq!(snap.messages[1].text(), "second");
    }

    // === 场景 5：restore 恢复快照后计数与快照一致 ===

    #[tokio::test]
    async fn restore_resets_to_snapshot_state() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 10_000, None);
        for i in 0..3u32 {
            mgr.append(Message::user_text(format!("msg {i}"))).await;
        }
        let snap = mgr.snapshot().await;
        // 快照后再追加 2 条
        mgr.append(Message::user_text("extra1")).await;
        mgr.append(Message::user_text("extra2")).await;
        assert_eq!(mgr.message_count(), 5);
        // 恢复快照
        mgr.restore(snap).await;
        assert_eq!(mgr.message_count(), 3);
        assert_eq!(mgr.token_count(), 30); // 3 条 × 10
    }

    // === 场景 6：build_chat_request 返回 ChatRequest，含 system prompt 和 tools schema ===

    #[tokio::test]
    async fn build_chat_request_returns_system_and_tools() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("test system prompt".into(), tokenizer, 10_000, None);
        mgr.append(Message::user_text("hello")).await;

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool::new("mock_tool", "a mock tool")));

        let config = RuntimeConfig::default();
        let req = mgr
            .build_chat_request(&tools, &config)
            .await
            .expect("build_chat_request 应成功");
        assert_eq!(req.system, "test system prompt");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "mock_tool");
    }

    // === 场景 7：build_chat_request 在 token 超阈值时触发压缩 ===

    #[tokio::test]
    async fn build_chat_request_triggers_compression_when_over_threshold() {
        // context_window=6000 → usable=880 → threshold=748
        // 100 条 × 10 字符 = 1000 tokens > 748，触发压缩
        // L3 rolling 保留 20 条 → 200 tokens < 748，压缩成功
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 6_000, None);
        for _ in 0..100 {
            mgr.append(Message::user_text("0123456789")).await; // 10 字符 = 10 token
        }
        let threshold = TokenBudget::new(6_000).compact_threshold();
        assert!(
            mgr.token_count() > threshold,
            "压缩前应超阈值: {} > {}",
            mgr.token_count(),
            threshold
        );

        let tools = ToolRegistry::new();
        let config = RuntimeConfig::default();
        let req = mgr
            .build_chat_request(&tools, &config)
            .await
            .expect("压缩后 build_chat_request 应成功");
        // L3 rolling 保留 20 条非 system 消息
        assert!(
            req.messages.len() <= 20,
            "L3 rolling 后应保留 ≤20 条: {}",
            req.messages.len()
        );
        assert!(
            mgr.token_count() <= threshold,
            "压缩后应降至阈值下: {}",
            mgr.token_count()
        );
    }

    // === 场景 8：compress 在无 provider 时走 L1→L3→L4 降级链（L2 跳过）===

    #[tokio::test]
    async fn compress_without_provider_runs_degradation_chain() {
        // 无 provider：L2 跳过，L1（无 tool_result 不裁剪）→ L3 rolling
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 6_000, None);
        for _ in 0..100 {
            mgr.append(Message::user_text("0123456789")).await;
        }
        let tokens_before = mgr.token_count();
        let threshold = TokenBudget::new(6_000).compact_threshold();
        assert!(tokens_before > threshold);

        // compress 应成功（L2 跳过，L3 rolling 足以降至阈值下）
        mgr.compress(false)
            .await
            .expect("无 provider 时 compress 应成功");

        // L3 rolling 保留 20 条非 system 消息
        assert!(
            mgr.message_count() <= 20,
            "L3 rolling 后应保留 ≤20 条: {}",
            mgr.message_count()
        );
        assert!(
            mgr.token_count() < tokens_before,
            "压缩后 token 应减少: before={tokens_before}, after={}",
            mgr.token_count()
        );
    }

    // === 场景 9：compress 成功后 token_count 下降 ===

    #[tokio::test]
    async fn compress_reduces_token_count() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 6_000, None);
        // 50 条 × 30 字符 = 1500 tokens > 748
        for _ in 0..50 {
            mgr.append(Message::user_text("012345678901234567890123456789"))
                .await; // 30 字符
        }
        let tokens_before = mgr.token_count();
        assert!(tokens_before > TokenBudget::new(6_000).compact_threshold());

        mgr.compress(false).await.expect("compress 应成功");

        let tokens_after = mgr.token_count();
        assert!(
            tokens_after < tokens_before,
            "压缩后 token 应减少: before={tokens_before}, after={tokens_after}"
        );
    }
}
