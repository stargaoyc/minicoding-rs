//! 进程内子 Agent runner（B1，见 `design.md` §7.3）。
//!
//! [`InProcessSubagentRunner`] 经 [`minicoding_core::runtime::RuntimeBuilder`] 组装
//! 嵌套子 `Runtime` 并跑完整 Agent 循环，替换生产链路上恒为 `NotConfigured` 的
//! `NoopSubagentRunner` 兜底。生产组装点在 `crate::builder::build_runtime`：
//! 内层 runner 外包 `WorktreeSubagentRunner`（worktree 隔离装饰器）后注入。
//!
//! 深度防御（F1）：子 Agent 工具集经 [`build_child_registry`] 全新构造——只注册
//! readonly/write/shell/git/web 组，**物理不注册** `task.spawn`/`plan.exit`/
//! `task.*`/`memory.write`，即使 `spec.can_spawn_subagent` 被上层误置 `true`，
//! 子 Agent 也无派发工具可用（杜绝无限嵌套的第二道防线）。
//!
//! 扇出上限（F2）：进程级 `tokio::sync::Semaphore`（[`MAX_CONCURRENT_SUBAGENTS`]），
//! `try_acquire` 失败立即返回错误而非排队，防止父 Agent 无界并发派发耗尽资源。
//!
//! 权限与沙箱继承（C-01/C-22）：子 Runtime 复用父 `PermissionPolicy` +
//! `PermissionPrompter`（弹窗冒泡到同一 UI）与 `SandboxDriver`/`SandboxPolicy`
//! ——子 Agent 的副作用调用不因嵌套而绕过权限或失去 OS 沙箱。

use minicoding_context::ContextManagerImpl;
use minicoding_core::agent::SubagentRunner;
use minicoding_core::config::RuntimeConfig;
use minicoding_core::context::ContextManager;
use minicoding_core::model::{RuntimeError, SubagentResult, SubagentSpec};
use minicoding_core::policy::{PermissionPolicy, PermissionPrompter};
use minicoding_core::provider::{BoxFuture, LlmProvider, Tokenizer};
use minicoding_core::runtime::RuntimeBuilder;
use minicoding_core::sandbox::SandboxDriver;
use minicoding_core::storage::AuditSink;
use minicoding_core::tool::ToolRegistry;
use minicoding_storage::JsonlStorage;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// 进程内并发子 Agent 上限（F2 扇出上限）。
///
/// 第 5 个及之后的并发派发以 `RuntimeError::Config` 立即失败（不排队），父 Agent
/// 收到错误后可串行重试。经验值参考 Claude Code 的并发 subagent 预算。
pub const MAX_CONCURRENT_SUBAGENTS: usize = 4;

/// 子 Agent 结果摘要的最大字符数（超出截断并标注）。
const SUMMARY_MAX_CHARS: usize = 2000;

/// 截断标注（追加到被截断的摘要末尾）。
const SUMMARY_TRUNCATED_MARKER: &str = "\n[... truncated]";

/// 进程内子 Agent runner（B1）。
///
/// 持有构建嵌套 Runtime 所需的全部句柄；`spawn` 时按 `spec` 装配子 Runtime
/// （独立会话 id，持久化到父 `sessions_dir` 便于审计），跑完单轮后把
/// `TurnOutcome` 映射为 `SubagentResult`。
#[allow(clippy::module_name_repetitions)] // 与 SubagentRunner 家族命名保持一致
pub struct InProcessSubagentRunner {
    /// 主 LLM provider（与父会话共用同一实例及其 Retry 装饰器）。
    provider: Arc<dyn LlmProvider>,
    /// 分词器（父级克隆或 Tiktoken；供子上下文精确计数）。
    tokenizer: Arc<dyn Tokenizer>,
    /// 父 system prompt（`spec.system_prompt` 为空时的类型预设兜底）。
    system_prompt: String,
    /// 父 JSONL 存储（复用 `sessions_dir`；子会话独立 id 持久化便于审计）。
    storage: Arc<JsonlStorage>,
    /// 权限交互器（继承父，使权限弹窗冒泡到同一 UI，C-01）。
    prompter: Arc<dyn PermissionPrompter>,
    /// 权限策略（继承父，含 `ReplayPolicy` 包装后的最终值，C-02 黑名单不失效）。
    policy: Arc<dyn PermissionPolicy>,
    /// 审计 sink（继承父，权限决策照常落盘，AGENTS §5.5）。
    audit: Arc<dyn AuditSink>,
    /// OS 沙箱驱动 + 策略（继承父，C-22 第二道防线不下放）；`None` = 父未启用。
    sandbox: Option<(
        Arc<dyn SandboxDriver>,
        minicoding_core::sandbox::SandboxPolicy,
    )>,
    /// 父配置快照（子配置克隆后 `max_tool_iters` 减半、下限 10）。
    base_config: RuntimeConfig,
    /// 父工作目录（`spec.workdir` 缺省时使用）。
    workdir: camino::Utf8PathBuf,
    /// 并发闸门（F2）。信号量许可跨整个 `spawn` future 持有。
    semaphore: Arc<Semaphore>,
}

impl std::fmt::Debug for InProcessSubagentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessSubagentRunner")
            .field("max_concurrent", &MAX_CONCURRENT_SUBAGENTS)
            .finish_non_exhaustive()
    }
}

impl InProcessSubagentRunner {
    /// 构造 runner（句柄由 frontend 组装点注入，见 `builder.rs`）。
    ///
    /// # Arguments
    /// 各参数语义见结构体字段文档。
    #[allow(clippy::too_many_arguments)] // 组装入口，句柄逐一显式传入，聚合 struct 反而隐藏依赖
    #[must_use]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tokenizer: Arc<dyn Tokenizer>,
        system_prompt: String,
        storage: Arc<JsonlStorage>,
        prompter: Arc<dyn PermissionPrompter>,
        policy: Arc<dyn PermissionPolicy>,
        audit: Arc<dyn AuditSink>,
        sandbox: Option<(
            Arc<dyn SandboxDriver>,
            minicoding_core::sandbox::SandboxPolicy,
        )>,
        base_config: RuntimeConfig,
        workdir: camino::Utf8PathBuf,
    ) -> Self {
        Self {
            provider,
            tokenizer,
            system_prompt,
            storage,
            prompter,
            policy,
            audit,
            sandbox,
            base_config,
            workdir,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_SUBAGENTS)),
        }
    }

    /// 子 Agent 工具集清单（测试断言用）。
    ///
    /// 与 `spawn` 实际注册给子 Runtime 的工具集完全一致——断言此处不含
    /// `task.spawn` 即等价于断言子 Agent 无法再派发（深度防御 F1 的可观测面）。
    #[must_use]
    pub fn child_tool_names(&self) -> Vec<String> {
        let reg = build_child_registry();
        let mut names: Vec<String> = reg.schemas().into_iter().map(|s| s.name).collect();
        names.sort();
        names
    }

    /// 子配置：克隆父配置并把 `max_tool_iters` 减半（下限 10）。
    ///
    /// 子 Agent 单任务迭代预算应小于主循环；减半 + 下限保证小预算父配置的子
    /// Agent 至少还有可用迭代轮次。
    fn child_config(&self) -> RuntimeConfig {
        let mut cfg = self.base_config.clone();
        cfg.context.max_tool_iters = (cfg.context.max_tool_iters / 2).max(10);
        cfg
    }

    /// 解析子 Agent system prompt：`spec.system_prompt` 显式覆盖优先；
    /// 否则用父 prompt；两者皆空时退回内置默认。
    fn resolve_system_prompt(&self, spec: &SubagentSpec) -> String {
        if !spec.system_prompt.is_empty() {
            return spec.system_prompt.clone();
        }
        self.system_prompt.clone()
    }
}

/// 构造子 Agent 工具注册表（深度防御 F1）。
///
/// 全新 `ToolRegistry`，只注册 readonly/write/shell/git/web 五组：
/// - **不注册 `task.*` 管理组与 `TaskSpawn`**——后者本就只能经
///   `register_dynamic_tool` 注入，全新注册表天然缺失；
/// - 不注册 `plan.exit`（子 Agent 不应切换父 Plan 状态）；
/// - 不注册 `memory.write`（子 Agent 记忆写入由父汇总后执行，避免并发写冲突）。
///
/// 即使上层校验失误把 `can_spawn_subagent` 置 `true`，子 Agent 也无任何派发
/// 工具可调——能力从物理上移除而非仅靠约定。
fn build_child_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    minicoding_tools::register_readonly_tools(&mut tools);
    minicoding_tools::register_write_tools(&mut tools);
    minicoding_tools::register_shell_tools(&mut tools);
    minicoding_tools::register_git_tools(&mut tools);
    #[cfg(feature = "web")]
    minicoding_tools::register_web_tools(&mut tools);
    tools
}

impl SubagentRunner for InProcessSubagentRunner {
    fn spawn(
        &self,
        spec: SubagentSpec,
        input: String,
    ) -> BoxFuture<'_, Result<SubagentResult, RuntimeError>> {
        // 先取全部 Arc 克隆，async 块不借用 `&self`（trait 返回 `BoxFuture<'_>`，
        // 但克隆后 future 不依赖 self 生命周期，调试与组合更简单）。
        let provider = self.provider.clone();
        let tokenizer = self.tokenizer.clone();
        let storage = self.storage.clone();
        let prompter = self.prompter.clone();
        let policy = self.policy.clone();
        let audit = self.audit.clone();
        let sandbox = self.sandbox.clone();
        let child_config = self.child_config();
        let system_prompt = self.resolve_system_prompt(&spec);
        let workdir = spec.workdir.clone().unwrap_or_else(|| self.workdir.clone());
        let semaphore = self.semaphore.clone();

        Box::pin(async move {
            // F2：try_acquire 而非 acquire——满载立即报错让父 Agent 决策重试，
            // 而不是静默排队放大延迟（排队还会让父 turn 超时难以归因）。
            let _permit = semaphore.try_acquire().map_err(|_| {
                RuntimeError::Config(format!(
                    "并发子代理已达上限（{MAX_CONCURRENT_SUBAGENTS}），请稍后重试"
                ))
            })?;

            // 嵌套 Runtime：独立 ContextManager（无父历史）、独立 SessionId，
            // 共享存储目录/provider/policy/prompter/sandbox。
            let child_ctx: Arc<dyn ContextManager> = Arc::new(ContextManagerImpl::new(
                system_prompt,
                tokenizer.clone(),
                provider.capabilities().context_window,
                Some(provider.clone()),
            ));

            let mut builder = RuntimeBuilder::new()
                .provider(provider)
                .context(child_ctx.clone())
                .storage(storage.clone())
                .tools(build_child_registry())
                .config(child_config)
                .workdir(workdir)
                .policy(policy)
                .prompter(prompter)
                .audit(audit);
            if let Some((driver, sandbox_policy)) = sandbox {
                builder = builder
                    .sandbox_driver(driver)
                    .sandbox_policy(sandbox_policy);
            }
            let rt = builder.build()?;

            // Event Sourcing 初始化：新会话落 SessionCreated 事件（审计可回溯）。
            rt.init_event_stream().await?;

            // 单次子 Agent 执行超时（spec.timeout）：超时映射为 completed=false
            // 的结果而非 Err——超时是执行状态而非编程错误，父 Agent 应能拿到
            // 已消耗 token 并决定是否降级重试。
            let turn_fut = rt.run_turn(minicoding_core::model::UserInput::from_text(input));
            let outcome = tokio::time::timeout(spec.timeout, turn_fut).await;

            let (text, completed) = match outcome {
                // 正常结束 / 中断 / 失败三分映射：
                // - Finished → completed=true；
                // - Interrupted → completed=false（部分产出仍回传，C-05 只回传摘要）；
                // - Failed(e)/Err(e) → Err 上抛（LLM/存储等故障应让 task.spawn 报错可见）。
                Ok(Ok(minicoding_core::model::TurnOutcome::Finished(msg))) => (msg.text(), true),
                Ok(Ok(minicoding_core::model::TurnOutcome::Interrupted(msg))) => {
                    (msg.text(), false)
                }
                Ok(Ok(minicoding_core::model::TurnOutcome::Failed(e)) | Err(e)) => return Err(e),
                Err(_elapsed) => (
                    format!("子代理执行超时（{:?}），已中止", spec.timeout),
                    false,
                ),
            };

            // usage 累计：子上下文本轮全量 token 计数（独立上下文从零起算，
            // 即本轮 LLM+工具消息的累计输入规模）。
            let token_used = child_ctx.token_count();
            Ok(if completed {
                SubagentResult::completed(truncate_summary(&text), token_used)
            } else {
                SubagentResult::incomplete(truncate_summary(&text), token_used)
            })
        })
    }
}

/// 截断摘要至 `SUMMARY_MAX_CHARS` 字符（char 边界安全）。
fn truncate_summary(text: &str) -> String {
    if text.chars().count() <= SUMMARY_MAX_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(SUMMARY_MAX_CHARS).collect();
    out.push_str(SUMMARY_TRUNCATED_MARKER);
    out
}

/// 兜底分词器：字符计数（Tiktoken 构造失败时子代理降级用）。
///
/// 仅影响 token 预算估算精度（高估 CJK、低估英文词），不影响执行正确性；
/// 避免因分词器不可用导致子 Agent 能力整体不可用。
#[derive(Debug, Default)]
struct CharCountTokenizer;

impl Tokenizer for CharCountTokenizer {
    fn count(&self, text: &str) -> usize {
        text.chars().count()
    }

    fn count_messages(&self, msgs: &[minicoding_core::model::Message]) -> usize {
        msgs.iter().map(|m| self.count(&m.text())).sum()
    }

    fn id(&self) -> &'static str {
        "char-count-fallback"
    }
}

/// 构造兜底分词器（builder 在 Tiktoken 不可用时注入，见 `builder.rs` 11b-0）。
#[must_use]
pub fn fallback_tokenizer() -> Arc<dyn Tokenizer> {
    Arc::new(CharCountTokenizer)
}

#[cfg(test)]
mod tests {
    //! `truncate_summary` 边界与 CJK 安全、子注册表裁剪（F1 可观测面）。

    use super::*;

    #[test]
    fn truncate_summary_short_passes_through() {
        assert_eq!(truncate_summary("ok"), "ok");
    }

    #[test]
    fn truncate_summary_cjk_is_char_boundary_safe() {
        // 3000 个汉字（9000 字节）必须按字符数截断而非字节切片（会 panic）。
        let text = "汉".repeat(3000);
        let out = truncate_summary(&text);
        assert_eq!(
            out.chars().count(),
            SUMMARY_MAX_CHARS + SUMMARY_TRUNCATED_MARKER.chars().count()
        );
        assert!(out.ends_with(SUMMARY_TRUNCATED_MARKER));
    }

    #[test]
    fn child_registry_excludes_dispatch_and_plan_tools() {
        // F1 深度防御回归：子工具集物理不含 task.spawn / plan.exit / memory.write。
        let reg = build_child_registry();
        assert!(reg.get("fs.read").is_some(), "只读工具应保留");
        assert!(reg.get("task.spawn").is_none(), "task.spawn 必须缺席");
        assert!(reg.get("plan.exit").is_none(), "plan.exit 必须缺席");
        assert!(reg.get("memory.write").is_none(), "memory.write 必须缺席");
    }
}
