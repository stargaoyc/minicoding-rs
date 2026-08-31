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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use camino::Utf8PathBuf;
use minicoding_core::config::RuntimeConfig;
use minicoding_core::context::{ContextManager, ContextSnapshot};
use minicoding_core::hooks::{DispatchConfig, HookEvent, HookInput, HookRegistry};
use minicoding_core::metrics;
use minicoding_core::model::{Message, RuntimeError};
use minicoding_core::otel::span_name;
use minicoding_core::prompt::{PromptContext, PromptPipeline};
use minicoding_core::provider::{BoxFuture, ChatRequest, GenerationParams, LlmProvider, Tokenizer};
use minicoding_core::storage::{AuditKind, AuditRecord, AuditSink};
use minicoding_core::tool::ToolRegistry;
use tokio::sync::Mutex;

use crate::budget::TokenBudget;
use crate::compress::{
    CircuitBreaker, CircuitBreakerConfig, CompressResult, PostCompactConfig, PredictiveTracker,
    StateKeep, SummarizeConfig, compress_pipeline, extract_read_files, inject_post_compact,
    should_predict_compact,
};

/// R9 CTX-2：低估检测常量——连续 3 次实际用量超本地估算 20% 触发压缩阈值收紧。
const UNDERESTIMATE_TRIGGER: usize = 3;
/// R9 CTX-2：低估判定的整数百分比阈值（实际 ≥ 估算 × 120% 视为低估）。
const UNDERESTIMATE_RATIO_PCT: usize = 120;

/// 根据压缩结果计算压缩级别（0-4，用于 metrics 维度）。
///
/// 级别语义：`0`=无操作；`1`=L1 裁剪；`2`=L2 摘要；`3`=L3 滚动；`4`=L4 硬截断。
/// 取最高级别（多级同时触发时反映"最激进"的压缩手段）。
fn compress_level(result: &CompressResult) -> u8 {
    if result.truncated_count > 0 {
        4
    } else if result.dropped_count > 0 {
        3
    } else if result.summarized_count > 0 {
        2
    } else {
        u8::from(result.clipped_count > 0)
    }
}

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
    // R9 CTX-2：连续低估计数器（`calibrate` 检测到 provider 实际用量持续高于
    // 本地估算时递增，累积到阈值后收紧压缩触发比例——防"低估 → 压缩触发晚
    // → 真实 400 → force_compress → 熔断"的静默降级路径）。
    underestimate_streak: AtomicUsize,
    // M-07（R-02）：消息序号锚点——append 计数，供压缩追溯区间推算
    // （每条 append 的消息占一个连续序号，与事件流 seq 的关系见
    // `CompressedRange` 文档；Step 事件不占消息序号）。
    append_seq: AtomicU64,
    // M-07（R-02）：压缩审计 sink（可选注入；未注入时压缩不打审计）。
    audit: Option<Arc<dyn AuditSink>>,
    // M-07（R-02）：会话 id（`set_session_hint` 由 Runtime 构造时注入）。
    // std Mutex：`set_session_hint` 是 sync trait 方法，不能 await tokio 锁。
    session_id: std::sync::Mutex<Option<String>>,
    // CTX-8（2026-08-25 R2 审查）：L2 摘要配置——此前 compress/mod.rs 硬编码
    // `SummarizeConfig::default()`，llm_timeout_secs 端到端不可配。
    summarize_config: SummarizeConfig,
    // CT4-2（R4）：system prompt + tool schemas 固定开销（token）——build_chat_request
    // 触发判定时计算并缓存，compress 的成功/超窗判据复用。压缩目标此前只看
    // messages-only，fixed_overhead 大时压缩"成功"后有效用量仍超阈值（且熔断被
    // record_success 重置、每轮白烧一次 L2 摘要）。
    fixed_overhead: AtomicUsize,
    // R8：Hook 注册表（PreCompact/PostCompact 事件派发用，见 `hooks.md` §2）。
    // 可选注入——未注入时压缩管道内无 Hook 触发点（与旧行为一致）。
    hook_registry: Option<Arc<dyn HookRegistry>>,
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
        // R10-10：预留输出由 provider 声明的 `max_output` 驱动（取 min(声明值,
        // 窗口 25%)，`with_reserved_output` 内部 clamp）——此前硬编码 4096，
        // 与 Anthropic 64K max_output 脱钩：可用预算被低估/高估都可能触发真实
        // 400（输入超阈值但输出预留不足）。
        let mut budget = TokenBudget::new(context_window);
        if let Some(p) = &provider {
            budget = budget.with_reserved_output(p.capabilities().max_output);
        }
        Self {
            messages: tokio::sync::RwLock::new(Vec::new()),
            system_prompt,
            pipeline: None,
            prompt_ctx_template: None,
            tokenizer,
            budget,
            provider,
            count: AtomicUsize::new(0),
            token_cache: AtomicUsize::new(0),
            circuit_breaker: Mutex::new(CircuitBreaker::new()),
            predictive_tracker: Mutex::new(PredictiveTracker::new()),
            underestimate_streak: AtomicUsize::new(0),
            append_seq: AtomicU64::new(0),
            audit: None,
            session_id: std::sync::Mutex::new(None),
            summarize_config: SummarizeConfig::default(),
            fixed_overhead: AtomicUsize::new(0),
            hook_registry: None,
        }
    }

    /// 注入压缩审计 sink（M-07，R-02）。
    ///
    /// 未注入时压缩流程不落 `AuditKind::Compress` 审计（如测试场景）。
    pub fn set_audit(&mut self, audit: Arc<dyn AuditSink>) {
        self.audit = Some(audit);
    }

    /// 压缩前最后一条消息的序号（`None` = 尚无消息）。
    fn last_append_seq(&self) -> Option<u64> {
        let n = self.append_seq.load(Ordering::SeqCst);
        (n > 0).then_some(n)
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

    /// 注入压缩熔断器配置（CT-4，2026-08-25 审查）。
    ///
    /// 默认 [`CircuitBreakerConfig::default`]（fail=3 / `force_end=5` / thrash=2 /
    /// cooldown=60s）。阈值配置化的终态是挂到 `core::ContextConfig` 字段由 CLI
    /// 层透传（core 本轮只读不改），在该扩展落地前以链式 builder 提供注入点，
    /// 不破坏既有构造签名（server/sdk 调用点零改动，行为默认不变）。
    #[must_use]
    pub fn with_circuit_breaker_config(mut self, config: CircuitBreakerConfig) -> Self {
        self.circuit_breaker = Mutex::new(CircuitBreaker::with_config(config));
        self
    }

    /// 注入 L2 摘要配置（CTX-8，2026-08-25 R2 审查）：`ratio`/`max_summary_tokens`
    /// /`llm_timeout_secs` 由调用方透传，不再硬编码 default。
    #[must_use]
    pub fn with_summarize_config(mut self, config: SummarizeConfig) -> Self {
        self.summarize_config = config;
        self
    }

    /// 链式设置压缩触发比例（CTX-R6-7，2026-08-28 R6 审查）：由
    /// `config.context.budget_ratio` 驱动——此前 `TokenBudget` 硬编码 0.85，
    /// 配置字段零消费（死配置）。`with_ratio` 内部 clamp 到 [0.1, 1.0]。
    #[must_use]
    pub fn with_budget_ratio(mut self, ratio: f64) -> Self {
        self.budget = self.budget.with_ratio(ratio);
        self
    }

    /// 注入 Hook 注册表（R8：#1 PreCompact/PostCompact 事件接线，`hooks.md` §2）。
    ///
    /// 注入后 `compress()` 在压缩管道启动前派发 `PreCompact`（extras 携带
    /// `tokens_before`），成功后派发 `PostCompact`（extras 携带
    /// `tokens_before`/`tokens_after`）。未注入时无 Hook 触发点（与旧行为一致，
    /// 零开销）。Hook 仅观察/注入（`inject_context`），不参与压缩决策（C-29
    /// 熔断状态机由 Runtime 判定，与 Hook 无关）。
    #[must_use]
    pub fn with_hook_registry(mut self, registry: Option<Arc<dyn HookRegistry>>) -> Self {
        self.hook_registry = registry;
        self
    }

    /// 派发压缩生命周期 Hook（R8：#1）。
    async fn dispatch_compress_hook(&self, event: HookEvent, extras: serde_json::Value) {
        let Some(registry) = &self.hook_registry else {
            return;
        };
        let session = self.session_id.lock().expect("session_id poisoned").clone();
        // R8 CTX-3 修复：Hook 子进程 cwd 应取会话 workdir（`prompt_ctx_template`
        // 携带），而非进程级 `std::env::current_dir()`——server 多会话模式下进程
        // cwd 与当前会话 workdir 可能不一致（此前压缩 Hook 的 cwd 字段失真）。
        let cwd = self.prompt_ctx_template.as_ref().map_or_else(
            || {
                std::env::current_dir()
                    .ok()
                    .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
                    .unwrap_or_else(|| Utf8PathBuf::from("."))
            },
            |t| t.workdir.clone(),
        );
        let input = HookInput::new(event, session.unwrap_or_default(), 0, cwd);
        // extras 手动填充（HookInput::new 的 extras 为 Null）
        let input = HookInput { extras, ..input };
        // 忽略 Hook 输出：PreCompact/PostCompact 的 inject_context 由 Runtime 的
        // pending_hook_contexts 缓冲消费，此处仅派发通知（C-21 不可影响压缩决策）。
        let _ = registry.dispatch(input, DispatchConfig::default()).await;
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

    /// 压缩成功审计（M-07，R-02）：记录级别、压缩区间、掉 token 量。
    ///
    /// 未注入 audit sink 时为 no-op。detail 为 JSON 文本，便于审计侧结构化检索。
    async fn record_compress_audit(
        &self,
        result: &CompressResult,
        level: u8,
        tokens_before: usize,
        tokens_after: usize,
    ) {
        let Some(audit) = &self.audit else {
            return;
        };
        let detail = serde_json::json!({
            "level": level,
            "clipped_count": result.clipped_count,
            "summarized_count": result.summarized_count,
            "dropped_count": result.dropped_count,
            "truncated_count": result.truncated_count,
            "fallback_used": result.fallback_used,
            "dropped_range": result.dropped_range.map(|(from, to)| {
                serde_json::json!({ "from_seq": from, "to_seq": to })
            }),
            "dropped_tokens": result.dropped_tokens,
            "tokens_before": tokens_before,
            "tokens_after": tokens_after,
        })
        .to_string();
        let rec = AuditRecord {
            ts: time::OffsetDateTime::now_utc(),
            session: self
                .session_id
                .lock()
                .expect("session_id poisoned")
                .clone()
                .unwrap_or_default(),
            kind: AuditKind::Compress,
            tool: None,
            decision: None,
            detail,
        };
        if let Err(e) = audit.record(rec).await {
            tracing::warn!(error = %e, "compress audit record failed (best-effort)");
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
    #[tracing::instrument(
        skip(self),
        fields(
            otel.name = span_name::COMPRESS,
            compress.tokens_before = tracing::field::Empty,
            compress.tokens_after = tracing::field::Empty,
        )
    )]
    pub async fn compress(&self) -> Result<(), RuntimeError> {
        let state_keep = StateKeep::snapshot(&self.system_prompt);
        let provider_ref = self.provider.as_deref();

        // R8：#1 PreCompact Hook——压缩管道启动前派发（extras 携带压缩前 token
        // 预估值；tokens_after 未知，由 PostCompact 携带）。
        let tokens_estimate_before = self.token_count();
        self.dispatch_compress_hook(
            HookEvent::PreCompact,
            serde_json::json!({ "tokens_before": tokens_estimate_before }),
        )
        .await;

        // 持写锁运行压缩管道，释放后再获取熔断器锁（避免锁序倒置：messages → breaker）。
        //
        // CT-1（2026-08-25 审查）：写锁跨整个管道含 L2 的 LLM 摘要调用。曾评估
        // "读快照→放锁→调用→回写"的收窄方案，不可行：放锁窗口内并发 append
        // （尾插）/restore（整表换血）与回写竞态会丢失增量，正确合并需引入消息
        // 版本号协议，超出缺陷修复范围。现以 fallback.rs 的 30s 超时兜底封顶
        // 锁的最长持有时长（主/备 provider 各一次）。
        let (outcome, new_tokens, tokens_before) = {
            let mut guard = self.messages.write().await;
            let tokens_before = self.tokenizer.count_messages(&guard);
            let anchor_seq = self.last_append_seq();
            let outcome = compress_pipeline(
                &mut guard,
                self.tokenizer.as_ref(),
                &self.budget,
                provider_ref,
                anchor_seq,
                &self.summarize_config,
            )
            .await;
            // 压缩后重算 token 缓存（messages 可能已变更）
            let tokens = self.tokenizer.count_messages(&guard);
            self.token_cache.store(tokens, Ordering::SeqCst);
            self.count.store(guard.len(), Ordering::SeqCst);
            (outcome, tokens, tokens_before)
        }; // 写锁释放

        // Span 属性：动态记录压缩前后 token 数
        let span = tracing::Span::current();
        span.record("compress.tokens_before", tokens_before);
        span.record("compress.tokens_after", new_tokens);

        // Metrics：压缩前后 token 分布（histogram）
        metrics::record_context_tokens("before_compress", tokens_before as u64);
        metrics::record_context_tokens("after_compress", new_tokens as u64);

        // CTX-4（2026-08-25 R2 审查）：审计落盘在熔断器锁**之外**——audit 是
        // 文件 IO await，此前在 breaker 锁内执行，并发 build_chat_request 的
        // 预检会被无谓阻塞。level 在两个 match 分支各自重算（纯函数，开销可忽略）。
        if let Ok(result) = &outcome {
            let level = compress_level(result);
            self.record_compress_audit(result, level, tokens_before, new_tokens)
                .await;
            // R8：#1 PostCompact Hook——压缩成功后派发（extras 携带压缩前后 token 数）。
            self.dispatch_compress_hook(
                HookEvent::PostCompact,
                serde_json::json!({
                    "tokens_before": tokens_before,
                    "tokens_after": new_tokens,
                }),
            )
            .await;
        }

        // R9 CTX-2：触发阈值改用 `effective_compact_threshold()`——检测到持续
        // 低估（calibrate 累计 streak）时提前触发压缩，防"触发晚 → 真实 400 →
        // force_compress → 熔断"静默降级路径。
        let threshold = self.effective_compact_threshold();
        let fixed_overhead = self.fixed_overhead.load(Ordering::SeqCst);
        let effective_threshold = threshold.saturating_sub(fixed_overhead);
        let mut breaker = self.circuit_breaker.lock().await;

        match outcome {
            Ok(result) => {
                let level = compress_level(&result);
                // CT4-2（R4）：成功判据使用有效阈值（扣除 fixed_overhead）——
                // 此前只看 messages-only，fixed_overhead 大时压缩"成功"后实际
                // 请求仍超窗，且熔断被 record_success 重置。
                //
                // CTX-2（R5）：`effective_threshold == 0` 表示 system prompt +
                // 工具 schemas 本身已 ≥ 完整阈值——小窗口模型下任何压缩都无济于事，
                // 此时判定"超阈值"会让**每**次压缩都 oversize → thrash 熔断，
                // 会话连第一条消息都发不出（无降级路径）。改为：窗口装不下固定
                // 开销时跳过 oversize 分支（记录 normal，交由 build_chat_request
                // 侧的能力降级：post_compact 预算随剩余窗口收缩）。
                if effective_threshold > 0 && new_tokens > effective_threshold {
                    // CT4-2（R4）：used 报有效用量（messages + fixed_overhead），
                    // budget 报原始阈值——此前 used 只报 messages 误导排障
                    Self::handle_oversize(&mut breaker, level, new_tokens, effective_threshold)?;
                } else {
                    breaker.record_success();
                    metrics::record_compress(level, "ok");
                    metrics::set_circuit_breaker("compress", "normal");
                }
            }
            Err(e) => {
                breaker.record_failure();
                metrics::record_error("context");
                if breaker.should_force_end() {
                    tracing::warn!(
                        fail_count = breaker.fail_count(),
                        "压缩熔断：失败计数 ≥ 5，强制 TurnEnd"
                    );
                    metrics::record_compress(0, "err");
                    metrics::set_circuit_breaker("compress", "force_end");
                    return Err(RuntimeError::BudgetExceeded {
                        used: new_tokens.saturating_add(fixed_overhead),
                        budget: threshold,
                    });
                }
                if breaker.should_trip() {
                    tracing::warn!(
                        fail_count = breaker.fail_count(),
                        "压缩熔断：失败计数 ≥ 3，中止本轮"
                    );
                    metrics::record_compress(0, "err");
                    metrics::set_circuit_breaker("compress", "fused");
                    return Err(RuntimeError::BudgetExceeded {
                        used: new_tokens.saturating_add(fixed_overhead),
                        budget: threshold,
                    });
                }
                // fail_count < 3：传播原始错误（降级链已在 pipeline 内尝试）
                tracing::warn!(
                    fail_count = breaker.fail_count(),
                    "压缩失败但未达熔断阈值，传播错误"
                );
                metrics::record_compress(0, "err");
                return Err(e);
            }
        }

        state_keep.assert_unchanged(&self.system_prompt);
        Ok(())
    }

    /// 处理"压缩成功但 token 仍超阈值"分支（Thrash 检测）。
    ///
    /// 返回 `Err(BudgetExceeded)` 表示 Thrash 熔断触发（中止本轮）；
    /// 返回 `Ok(())` 表示未熔断（继续发送，调用方在外层完成 breaker 锁释放后继续）。
    fn handle_oversize(
        breaker: &mut CircuitBreaker,
        level: u8,
        new_tokens: usize,
        threshold: usize,
    ) -> Result<(), RuntimeError> {
        breaker.record_oversize();
        if breaker.is_thrashing() {
            tracing::warn!(
                fail_count = breaker.fail_count(),
                consecutive_oversize = breaker.consecutive_oversize(),
                "压缩熔断：Thrash 检测触发（连续超阈值），中止本轮"
            );
            metrics::record_compress(level, "err");
            metrics::set_circuit_breaker("compress", "fused");
            metrics::record_error("context");
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
        metrics::record_compress(level, "oversize");
        metrics::set_circuit_breaker("compress", "warning");
        Ok(())
    }

    /// R9 CTX-2：压缩触发阈值（考虑低估收紧）。
    ///
    /// 正常情况下与 `budget.compact_threshold()` 一致；检测到持续低估（
    /// [`ContextManager::calibrate`] 累计 streak）时按低估强度提前触发压缩——
    /// 低估方向越强触发越早，避免"压缩触发晚 → 真实 400 → `force_compress` →
    /// 熔断"静默降级路径。
    #[must_use]
    fn effective_compact_threshold(&self) -> usize {
        let streak = self.underestimate_streak.load(Ordering::SeqCst);
        let base = self.budget.compact_threshold();
        if streak == 0 {
            return base;
        }
        // 每连续低估 3 次收紧 10%，最多收紧 40%（低估严重时几乎每轮都收紧）。
        // 用整数百分比运算避免 f64 精度/截断告警。
        let tighten_pct = (streak / 3).min(4) * 10;
        base.saturating_mul(100 - tighten_pct) / 100
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
    fn set_session_hint(&self, id: &str) {
        *self.session_id.lock().expect("session_id poisoned") = Some(id.to_string());
    }

    /// 校准：`actual` 反映刚发送请求的真实 prompt 规模，直接覆盖缓存
    /// （比增量估算可信）；指数平滑系数 0.5 抑制单次异常。
    ///
    /// CTX-3（2026-08-26 R3 审查）护栏：`actual == 0` 直接跳过——部分
    /// provider/解析缺失路径会产出零值 Usage，midpoint(current, 0) 会把
    /// 缓存直接砍半（低估 → 该压缩时不压缩 → 真实超窗）。另注意口径差：
    /// `actual` 含 system+tools 固定开销而缓存仅 messages，混入 midpoint 会
    /// 系统性高估基数（方向保守、浪费预算但安全），此处保留并文档化。
    fn calibrate(&self, actual_input_tokens: usize) {
        if actual_input_tokens == 0 {
            tracing::debug!("calibrate skipped: zero actual (provider usage missing)");
            return;
        }
        // R9 CTX-1 修复：`actual`（provider 返回的完整请求用量）含 system +
        // tools schema 固定开销，而 `token_cache` 仅计 messages——直接混合
        // 会让校准值系统性高于消息量（工具多的会话固定开销数千 token），
        // 导致过早压缩（浪费预算）。校准前扣减本次请求的固定开销基线
        // （`fixed_overhead` 由 `build_chat_request` 触发判定时计算缓存）。
        let fixed = self.fixed_overhead.load(Ordering::SeqCst);
        let messages_actual = actual_input_tokens.saturating_sub(fixed);
        let current = self.token_cache.load(Ordering::SeqCst);
        let blended = usize::midpoint(current, messages_actual);
        self.token_cache.store(blended, Ordering::SeqCst);
        // R9 CTX-2：低估检测——provider 实际用量（扣固定开销后）持续高于本地
        // 估算（token_cache），说明分词器/估算系统性低估（压缩触发过晚 → 真实
        // 400 → force_compress → 熔断的静默降级路径）。连续
        // [`ContextManagerImpl::UNDERESTIMATE_TRIGGER`] 次超 20% 后，
        // `effective_compact_threshold` 按低估强度收紧触发阈值（提前压缩，而非
        // 等真实 400）。整数百分比比较避免浮点转换（CLIPPY pedantic）。
        if messages_actual > 0 && current > 0 {
            let ratio_pct = messages_actual.saturating_mul(100) / current;
            if ratio_pct > UNDERESTIMATE_RATIO_PCT {
                let streak = self.underestimate_streak.fetch_add(1, Ordering::SeqCst) + 1;
                if streak >= UNDERESTIMATE_TRIGGER {
                    tracing::warn!(
                        streak,
                        estimate = current,
                        actual = messages_actual,
                        "上下文 token 估算连续低估（>20%），已收紧压缩触发阈值（防真实 400 熔断）"
                    );
                }
            } else {
                self.underestimate_streak.store(0, Ordering::SeqCst);
            }
        }
        tracing::debug!(
            current,
            actual = actual_input_tokens,
            fixed_overhead = fixed,
            messages_actual,
            blended,
            "token cache calibrated"
        );
    }

    fn append(&self, msg: Message) -> BoxFuture<'_, ()> {
        // 增量计算新消息 token（含消息框架开销），append 后加到缓存。
        // CTX-4（2026-08-27 R5 审查）：`count_messages` 在末尾统一加
        // `TOKENS_REPLY_PRIMING`（3 token，单次消息列表开销），逐条 append
        // 调用时每消息多计 3 token——N 条后缓存虚高 3×(N-1)，系统性提前触发
        // 压缩（方向安全但浪费预算）且 `snapshot().token_count` 与 `token_count()`
        // 不一致。`count_messages(&[])` 返回独立 priming 量，delta 扣除该值。
        let delta = self.tokenizer.count_messages(std::slice::from_ref(&msg));
        let priming = self.tokenizer.count_messages(&[]);
        let delta_no_priming = delta.saturating_sub(priming);
        Box::pin(async move {
            // CTX-3（2026-08-28 R5 收尾）：先更新 token_cache 再 push 消息——
            // 此前 push 后 fetch_add 间存在锁外窗口：并发 compress 读 token_cache
            // 含新消息但未计入对应 token，预算系统性低估（超窗风险）。先更新
            // 缓存再 push 使 compress 看到的 cache ≥ 真实值（方向保守，提前触发
            // 压缩而非超窗）。
            //
            // CTX-R6-2（2026-08-28 R6 审查）：增量更新必须与 push 一起在写锁内
            // 原子完成——R5 修复后 fetch_add 仍在锁外，compress 的全量重算
            // （`store` 覆盖缓存，见 compact/restore 路径）可插在 fetch_add 与
            // push 之间：cache 被覆盖为"不含新消息"的旧值 → push 后缓存比真实
            // 值少一条消息的 token（低估方向，可致超窗）。移入写锁后与 push
            // 串行，compress 的 store 不可能吞掉本消息的增量。
            let mut guard = self.messages.write().await;
            self.token_cache
                .fetch_add(delta_no_priming, Ordering::SeqCst);
            self.count.fetch_add(1, Ordering::SeqCst);
            guard.push(msg);
            // M-07：消息序号锚点递增（压缩追溯区间推算基准）
            self.append_seq.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[tracing::instrument(skip(self, tools, config), fields(otel.name = span_name::CONTEXT_BUILD))]
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

            // Metrics：请求阶段 token 分布
            metrics::record_context_tokens("request", current_tokens as u64);

            // CTX-2（2026-08-26 R3 审查）：阈值判定计入 **system prompt 与
            // tool schemas** 固定开销——project_doc 可达 32KiB（≈8K token）、
            // 多工具 schemas 数千 token，此前只统计 messages，小窗口模型即使
            // 压到"阈值下"实际请求仍超窗。system 构建提前到触发判定之前。
            let base_system = self.build_base_system_prompt(&tool_schemas).await?;
            let tools_fixed = tool_schemas
                .iter()
                .map(|t| {
                    self.tokenizer.count(&t.name)
                        + self.tokenizer.count(&t.description)
                        + serde_json::to_string(&t.input_schema)
                            .map(|j| self.tokenizer.count(&j))
                            .unwrap_or_default()
                })
                .sum::<usize>();
            let fixed_overhead = self.tokenizer.count(&base_system) + tools_fixed;
            // CT4-2（R4）：缓存固定开销供 compress 成功判据复用（见字段注释）
            self.fixed_overhead.store(fixed_overhead, Ordering::SeqCst);
            let effective_tokens = current_tokens + fixed_overhead;

            // C-08：预测性压缩——当前未超阈值但预测下一 turn 会超时提前压缩
            // CTX-R6-8：与 reactive 判据同口径，计入 fixed_overhead
            let need_predictive = predictive_enabled && effective_tokens <= threshold && {
                let tracker = self.predictive_tracker.lock().await;
                should_predict_compact(
                    current_tokens,
                    threshold,
                    fixed_overhead,
                    &tracker,
                    predictive_baseline,
                )
            };

            // 检查是否触发压缩阈值（缓存计数，无需加锁）；超阈值先压缩再读消息，
            // 避免 compress 的写锁与下方读锁死锁（RwLock 不可重入）。
            // compress=off 时跳过压缩直通（C-18 软约束，用户显式关闭）。
            //
            // CT-5 残留修复（2026-08-25 R2 审查）：read 路径在压缩**前**提取——
            // 此后在压缩后的历史里提取，恰在 L3/L4 丢弃 fs.read 消息、最需要
            // 恢复的场景提取恒空（design §3.10 要求的独立环形缓冲仍列为后续项，
            // 本修复保证"压缩当次"注入有效）。
            let will_compress =
                compress_enabled && (effective_tokens > threshold || need_predictive);
            let pre_compress_read_files = if will_compress {
                let guard = self.messages.read().await;
                let files = extract_read_files(&guard, post_compact_cfg.max_files);
                drop(guard);
                files
            } else {
                Vec::new()
            };
            let did_compress = if will_compress {
                // C-29：熔断状态机在 Runtime 层，压缩前检查是否已熔断。
                {
                    let breaker = self.circuit_breaker.lock().await;
                    if breaker.should_trip() || breaker.is_thrashing() {
                        tracing::warn!(
                            fail_count = breaker.fail_count(),
                            consecutive_oversize = breaker.consecutive_oversize(),
                            "压缩熔断已触发，拒绝 build_chat_request"
                        );
                        metrics::record_error("context");
                        metrics::record_compress(0, "skipped");
                        return Err(RuntimeError::BudgetExceeded {
                            used: current_tokens,
                            budget: threshold,
                        });
                    }
                } // 熔断器锁释放
                self.compress().await?;
                true
            } else {
                false
            };

            // 基础 system prompt 已在上方（CTX-2）提前构建——post-compact 注入
            // 在 base 之上叠加（无论 pipeline/static 都适用）。

            // C-09：post-compact 上下文恢复——压缩后重新注入最近读过的文件
            //（路径已在压缩前提取，见上方 CT-5 残留修复）
            let system = if did_compress {
                let read_files = pre_compress_read_files;
                if read_files.is_empty() {
                    base_system
                } else {
                    // CT-5（2026-08-25 审查）：相对路径基于会话 workdir 解析而非进程
                    // cwd——server/sdk 多会话下两者可能不同；模板未注入时（测试路径）
                    // 退回进程 cwd 兜底。
                    let workdir = self.prompt_ctx_template.as_ref().map_or_else(
                        || {
                            std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        },
                        |t| t.workdir.as_std_path().to_path_buf(),
                    );
                    // CTX-1（R5）：post_compact 注入预算必须服从**剩余窗口**——
                    // 此前独立用配置的 `post_compact_token_budget`（默认 50K token），
                    // 压缩"成功"判据（CT4-2，仅计 base_system+tools）漏计注入内容，
                    // 大文件重注入 + 小窗口模型下真实请求超窗且熔断检测不到。
                    // 现按 `threshold - (压缩后 messages + fixed_overhead)` 收缩
                    // token_budget（剩余窗口为 0 时跳过注入）。
                    let tokens_after_compress = self.token_count();
                    let remaining_window = threshold
                        .saturating_sub(tokens_after_compress.saturating_add(fixed_overhead));
                    let mut effective_cfg = post_compact_cfg.clone();
                    effective_cfg.token_budget = effective_cfg.token_budget.min(remaining_window);
                    if effective_cfg.token_budget == 0 {
                        tracing::debug!(remaining_window, "post-compact: 剩余窗口不足，跳过注入");
                        base_system
                    } else {
                        inject_post_compact(
                            &base_system,
                            &read_files,
                            &effective_cfg,
                            self.tokenizer.as_ref(),
                            &workdir,
                        )
                        .await
                    }
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
            // PTM-9：会话 id 作为 prompt_cache_key（OpenAI 缓存路由），提升
            // 长会话 prompt cache 命中率；其他 provider 忽略该字段。
            let params = GenerationParams {
                model,
                temperature: None,
                top_p: None,
                max_output_tokens: None,
                stop: Vec::new(),
                seed: None,
                thinking_budget_tokens: None,
                cache_key: self.session_id.lock().expect("session_id poisoned").clone(),
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
            // CTX-R6-4（2026-08-28 R6 审查）：重置消息序号锚点——`/clear` 走
            // restore（空快照）后 append_seq 若继续递增，压缩追溯区间（按
            // append_seq 推算）产生天文序号，审计日志的 CompressedRange 与
            // 事件流 seq 失准。锚点与消息数对齐（0-based：下一条 append 得 0）。
            self.append_seq.store(0, Ordering::SeqCst);
        })
    }

    fn token_count(&self) -> usize {
        self.token_cache.load(Ordering::SeqCst)
    }

    fn message_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// PT4-3（2026-08-28 R8 审查）：`force_compress` 走完整压缩管道 + 熔断/降级链
    /// （与 `compress()` 同实现——`compress` 已 `pub async fn`，trait 方法包装转发）。
    fn force_compress(&self) -> BoxFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async move { self.compress().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::config::RuntimeConfig;
    use minicoding_core::context::{ContextManager, ContextSnapshot};
    use minicoding_core::hooks::{DispatchResult, Hook};
    use minicoding_core::model::{
        Message, RuntimeError, SideEffect, ToolError, ToolResult, ToolSchema,
    };
    use minicoding_core::provider::Tokenizer;
    use minicoding_core::tool::{Tool, ToolContext, ToolRegistry};
    use std::sync::Arc;
    use std::sync::Mutex;

    use crate::budget::TokenBudget;
    use camino::Utf8PathBuf;
    use minicoding_core::model::PromptError;
    use minicoding_core::prompt::{
        PromptContext, PromptContributor, PromptPipeline, PromptSection, PromptSectionOrder,
    };

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
        mgr.compress()
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

        mgr.compress().await.expect("compress 应成功");

        let tokens_after = mgr.token_count();
        assert!(
            tokens_after < tokens_before,
            "压缩后 token 应减少: before={tokens_before}, after={tokens_after}"
        );
    }

    // === 场景 10：with_prompt_pipeline 注入后 build_chat_request 使用动态 system prompt ===

    /// 静态 contributor：返回固定 section（prompt pipeline 测试用）。
    struct StaticContributor {
        content: &'static str,
    }

    impl PromptContributor for StaticContributor {
        fn name(&self) -> &'static str {
            "test-identity"
        }
        fn order(&self) -> PromptSectionOrder {
            PromptSectionOrder::Identity
        }
        fn cacheable(&self) -> bool {
            true
        }
        fn build(&self, _ctx: &PromptContext) -> BoxFuture<'_, Result<PromptSection, PromptError>> {
            let content = self.content;
            Box::pin(async move {
                Ok(PromptSection {
                    contributor_name: "test-identity".into(),
                    content: content.into(),
                    order: PromptSectionOrder::Identity,
                    cacheable: true,
                    boundary: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn with_prompt_pipeline_overrides_static_system() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let mut pipeline = PromptPipeline::new();
        pipeline.register(Arc::new(StaticContributor {
            content: "dynamic system prompt from pipeline",
        }));
        let ctx = PromptContext::new("session-1".to_string(), Utf8PathBuf::from("/tmp"));
        let mgr = ContextManagerImpl::new("static fallback".into(), tokenizer, 10_000, None)
            .with_prompt_pipeline(Arc::new(pipeline), ctx);

        mgr.append(Message::user_text("hello")).await;
        let tools = ToolRegistry::new();
        let config = RuntimeConfig::default();
        let req = mgr
            .build_chat_request(&tools, &config)
            .await
            .expect("build_chat_request 应成功");
        assert!(
            req.system.contains("dynamic system prompt from pipeline"),
            "system 应来自 pipeline: {}",
            req.system
        );
        assert!(
            !req.system.contains("static fallback"),
            "不应回退到静态 system_prompt: {}",
            req.system
        );
    }

    // === 场景 10b：压缩成功落 Compress 审计（M-07，R-02）===

    #[tokio::test]
    async fn compress_records_compress_audit() {
        use minicoding_core::provider::BoxFuture;
        use minicoding_core::storage::{AuditKind, AuditRecord, AuditSink, StorageError};

        #[derive(Default)]
        struct InMemoryAudit(std::sync::Mutex<Vec<AuditRecord>>);
        impl AuditSink for InMemoryAudit {
            fn record(&self, rec: AuditRecord) -> BoxFuture<'_, Result<(), StorageError>> {
                self.0.lock().expect("poisoned").push(rec);
                Box::pin(async move { Ok(()) })
            }
        }

        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let audit = Arc::new(InMemoryAudit::default());
        // 30 条 × 200 字符 > 阈值 → L3 rolling 丢弃（无 provider 跳过 L2）
        let mut mgr = ContextManagerImpl::new("sys".into(), tokenizer, 6_000, None);
        mgr.set_audit(audit.clone());
        mgr.set_session_hint("sess-compress-1");
        for _ in 0..30 {
            mgr.append(Message::user_text("x".repeat(200))).await;
        }
        assert!(mgr.token_count() > TokenBudget::new(6_000).compact_threshold());

        mgr.compress().await.expect("compress 应成功");

        let records = audit.0.lock().expect("poisoned").clone();
        assert_eq!(records.len(), 1, "应落一条 Compress 审计");
        let rec = &records[0];
        assert!(matches!(rec.kind, AuditKind::Compress));
        assert_eq!(rec.session, "sess-compress-1");
        let detail: serde_json::Value =
            serde_json::from_str(&rec.detail).expect("detail 应为 JSON");
        assert!(
            detail["level"].as_u64().unwrap_or(0) > 0,
            "应记录压缩级别: {detail}"
        );
        assert!(
            detail["dropped_range"]["from_seq"].as_u64().is_some(),
            "应记录丢弃区间: {detail}"
        );
        assert!(
            detail["dropped_tokens"].as_u64().unwrap_or(0) > 0,
            "应记录掉 token 量: {detail}"
        );
        assert!(
            detail["tokens_before"].as_u64().unwrap_or(0)
                > detail["tokens_after"].as_u64().unwrap_or(0)
        );
    }

    // === 场景 11：system 消息无法被压缩丢弃，连续 compress 触发 thrash 熔断 ===

    #[tokio::test]
    async fn compress_throws_budget_exceeded_on_thrash() {
        // context_window=6000 → usable=880 → threshold=748
        // append 一条 1000 字符的 system 消息（> 748，且无法被 L3/L4 丢弃）
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 6_000, None);
        mgr.append(Message::system_text("x".repeat(1_000))).await;
        let threshold = TokenBudget::new(6_000).compact_threshold();
        assert!(
            mgr.token_count() > threshold,
            "压缩前应超阈值: {} > {}",
            mgr.token_count(),
            threshold
        );

        // 第一次 compress：consecutive_oversize=1，is_thrashing=false，不熔断
        mgr.compress().await.expect("第一次 compress 不应熔断");

        // 第二次 compress：consecutive_oversize=2，is_thrashing=true，熔断
        let res = mgr.compress().await;
        assert!(
            matches!(res, Err(RuntimeError::BudgetExceeded { .. })),
            "第二次 compress 应触发 thrash 熔断: {res:?}"
        );
    }

    // === 场景 12：thrash 状态下 build_chat_request 拒绝并返回 BudgetExceeded ===

    #[tokio::test]
    async fn build_chat_request_rejects_when_thrashing() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 6_000, None);
        mgr.append(Message::system_text("x".repeat(1_000))).await;

        // 触发 thrash：连续两次 compress（第二次返回 Err）
        let _ = mgr.compress().await;
        let _ = mgr.compress().await;

        let tools = ToolRegistry::new();
        let config = RuntimeConfig::default();
        let res = mgr.build_chat_request(&tools, &config).await;
        assert!(
            matches!(res, Err(RuntimeError::BudgetExceeded { .. })),
            "thrash 后 build_chat_request 应返回 BudgetExceeded"
        );
    }

    // === 场景 13：restore 空快照后消息与计数归零 ===

    #[tokio::test]
    async fn restore_empty_snapshot_clears_messages() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 10_000, None);
        for i in 0..5u32 {
            mgr.append(Message::user_text(format!("msg {i}"))).await;
        }
        assert_eq!(mgr.message_count(), 5);
        assert_eq!(mgr.token_count(), 50);

        let empty_snap = ContextSnapshot::default();
        mgr.restore(empty_snap).await;
        assert_eq!(mgr.message_count(), 0);
        assert_eq!(mgr.token_count(), 0);
    }

    // === 场景 14：snapshot 同步更新 token_cache ===

    #[tokio::test]
    async fn snapshot_updates_token_cache() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 10_000, None);
        mgr.append(Message::user_text("hello")).await;
        mgr.append(Message::user_text("world")).await;
        assert_eq!(mgr.token_count(), 20);

        // snapshot 内部会重算 token 并同步到 token_cache
        let snap = mgr.snapshot().await;
        assert_eq!(snap.token_count, 20);
        assert_eq!(mgr.token_count(), 20);
    }

    // === 场景 15：compress=false 时 build_chat_request 跳过压缩 ===

    #[tokio::test]
    async fn build_chat_request_skips_compression_when_disabled() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 6_000, None);
        // 100 条 × 10 字符 = 1000 tokens > 748 threshold
        for _ in 0..100 {
            mgr.append(Message::user_text("0123456789")).await;
        }
        let tokens_before = mgr.token_count();
        let threshold = TokenBudget::new(6_000).compact_threshold();
        assert!(tokens_before > threshold);

        let tools = ToolRegistry::new();
        let mut config = RuntimeConfig::default();
        config.context.compress = false;
        let req = mgr
            .build_chat_request(&tools, &config)
            .await
            .expect("compress=false 时应成功");
        // 未压缩：消息数不变，token 不变
        assert_eq!(req.messages.len(), 100, "compress=false 时不应裁剪消息");
        assert_eq!(
            mgr.token_count(),
            tokens_before,
            "compress=false 时 token 不应变化"
        );
    }

    // === 场景 16：build_chat_request 含多条 tool schema ===

    #[tokio::test]
    async fn build_chat_request_returns_multiple_tools() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 10_000, None);
        mgr.append(Message::user_text("hi")).await;

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MockTool::new("tool_a", "first tool")));
        tools.register(Arc::new(MockTool::new("tool_b", "second tool")));
        tools.register(Arc::new(MockTool::new("tool_c", "third tool")));

        let config = RuntimeConfig::default();
        let req = mgr
            .build_chat_request(&tools, &config)
            .await
            .expect("build_chat_request 应成功");
        assert_eq!(req.tools.len(), 3);
        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"tool_a"));
        assert!(names.contains(&"tool_b"));
        assert!(names.contains(&"tool_c"));
    }

    // === 场景 17：Debug 实现展示预算与计数 ===

    #[test]
    fn debug_format_shows_budget_and_counts() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("my system".into(), tokenizer, 10_000, None);
        let debug_str = format!("{mgr:?}");
        assert!(debug_str.contains("ContextManagerImpl"));
        assert!(debug_str.contains("my system"));
        assert!(debug_str.contains("message_count: 0"));
        assert!(debug_str.contains("token_cache: 0"));
    }

    // === 场景 18：append 后 restore 再 append 计数正确 ===

    #[tokio::test]
    async fn append_restore_append_maintains_correct_counts() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(TenTokensTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 10_000, None);
        mgr.append(Message::user_text("first")).await;
        mgr.append(Message::user_text("second")).await;
        let snap = mgr.snapshot().await;

        // 追加更多
        mgr.append(Message::user_text("third")).await;
        mgr.append(Message::user_text("fourth")).await;
        assert_eq!(mgr.message_count(), 4);
        assert_eq!(mgr.token_count(), 40);

        // restore 到 2 条
        mgr.restore(snap).await;
        assert_eq!(mgr.message_count(), 2);
        assert_eq!(mgr.token_count(), 20);

        // 再追加
        mgr.append(Message::user_text("fifth")).await;
        assert_eq!(mgr.message_count(), 3);
        assert_eq!(mgr.token_count(), 30);
    }

    // === R8：#1 PreCompact/PostCompact Hook 派发 ===

    /// 记录事件派发的 HookRegistry（验证 PreCompact/PostCompact 触发顺序）。
    #[derive(Default)]
    struct RecordingRegistry {
        events: Mutex<Vec<HookEvent>>,
    }

    impl HookRegistry for RecordingRegistry {
        fn register(&self, _hook: Arc<dyn Hook>) {}
        fn for_event(&self, _event: HookEvent) -> Vec<Arc<dyn Hook>> {
            Vec::new()
        }
        fn count(&self) -> usize {
            0
        }
        fn dispatch(
            &self,
            input: HookInput,
            _config: DispatchConfig,
        ) -> BoxFuture<'_, DispatchResult> {
            self.events.lock().expect("lock").push(input.event);
            Box::pin(async move { DispatchResult::default() })
        }
    }

    #[tokio::test]
    async fn compress_dispatches_pre_and_post_compact_hooks() {
        // R8：#1——注入 registry 后 compress 应依次派发 PreCompact（含
        // tokens_before extras）与 PostCompact（含 tokens_before/after）。
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let registry = Arc::new(RecordingRegistry::default());
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 6_000, None)
            .with_hook_registry(Some(registry.clone() as Arc<dyn HookRegistry>));
        mgr.set_session_hint("sess-hook-1");
        for _ in 0..30 {
            mgr.append(Message::user_text("x".repeat(200))).await;
        }
        assert!(mgr.token_count() > TokenBudget::new(6_000).compact_threshold());

        mgr.compress().await.expect("compress 应成功");

        let events = registry.events.lock().expect("lock").clone();
        assert!(
            events.iter().any(|e| matches!(e, HookEvent::PreCompact)),
            "应派发 PreCompact: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, HookEvent::PostCompact)),
            "应派发 PostCompact: {events:?}"
        );
        let pre_idx = events
            .iter()
            .position(|e| matches!(e, HookEvent::PreCompact))
            .expect("PreCompact 存在");
        let post_idx = events
            .iter()
            .position(|e| matches!(e, HookEvent::PostCompact))
            .expect("PostCompact 存在");
        assert!(
            pre_idx < post_idx,
            "PreCompact 必须先于 PostCompact: {events:?}"
        );
    }

    #[tokio::test]
    async fn compress_without_registry_no_hook_dispatch() {
        // 未注入 registry：compress 正常完成且无 Hook 派发（零开销路径）。
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 6_000, None);
        for _ in 0..30 {
            mgr.append(Message::user_text("x".repeat(200))).await;
        }
        mgr.compress().await.expect("compress 应成功");
    }

    // ===== R9 CTX-2：低估检测与触发阈值收紧 =====

    #[test]
    fn calibrate_tracks_underestimate_streak() {
        let tokenizer: Arc<dyn Tokenizer> = Arc::new(CharTokenizer);
        let mgr = ContextManagerImpl::new("sys".into(), tokenizer, 128_000, None);
        // 初始估算 1000 token；provider 实际持续报 10000（远超 20% 低估）。
        // midpoint 收敛使 current 逐次升高，actual=10000 在 cache 升至 7750
        // 前仍 >20%，足以累计到 3 次。
        mgr.token_cache.store(1_000, Ordering::SeqCst);
        mgr.calibrate(10_000);
        assert_eq!(mgr.underestimate_streak.load(Ordering::SeqCst), 1);
        mgr.calibrate(10_000);
        assert_eq!(mgr.underestimate_streak.load(Ordering::SeqCst), 2);
        // 第三次触发收紧（streak >= 3）
        mgr.calibrate(10_000);
        assert_eq!(mgr.underestimate_streak.load(Ordering::SeqCst), 3);
        // 有效阈值应低于 base（128000 * 0.85 = 108800，收紧 10% → ~97920）
        let base = TokenBudget::new(128_000).compact_threshold();
        let eff = mgr.effective_compact_threshold();
        assert!(
            eff < base,
            "低估 streak 后阈值应收紧: eff={eff} base={base}"
        );
        // 正常用量不累计（重置 streak）
        mgr.token_cache.store(10_000, Ordering::SeqCst);
        mgr.calibrate(11_000);
        assert_eq!(
            mgr.underestimate_streak.load(Ordering::SeqCst),
            0,
            "正常用量应重置低估 streak"
        );
    }
}
