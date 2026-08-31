//! `Tool` trait + `ToolContext` + `SideEffect`（见 `api.md` §3.3）。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容。

use crate::journal::Journal;
use crate::model::{SideEffect, ToolError, ToolResult, ToolSchema};
use crate::provider::BoxFuture;
use crate::sandbox::{SandboxDriver, SandboxPolicy};
use crate::tool::render::{RenderIntent, ToolOutputSchema};
use camino::Utf8PathBuf;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// 重导出 `CancellationToken`：上游 crate（如 `minicoding-cli`）构造 `ToolContext`
// 时需要它，但不应直接依赖 `tokio-util`（重依赖按 crate 隔离，AGENTS.md §3.5）。
pub use tokio_util::sync::CancellationToken;

/// 子进程环境变量白名单（C-04 凭证不下传子进程；单一事实来源，2026-08-23
/// 审查 §6-P1：此前 `shell.run` 私有一份、`ctx.env` 恒空，git/background 等
/// 子进程拿到完全空环境——缺 HOME 使 git 无法解析身份/全局配置）。
///
/// 仅传递基础环境变量；`OPENAI_API_KEY`/`ANTHROPIC_API_KEY`/`*_KEY`/`*_TOKEN`/
/// `*_SECRET` 等凭证变量绝不传递。
pub const SAFE_ENV_WHITELIST: &[&str] =
    &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "TMPDIR"];

/// 从当前进程环境按白名单提取安全子集（C-04）。
#[must_use]
pub fn sanitized_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    for name in SAFE_ENV_WHITELIST {
        if let Ok(value) = std::env::var(name) {
            env.insert((*name).to_string(), value);
        }
    }
    env
}

/// 工具执行上下文（每轮调用时构造）。
///
/// `sandbox_driver` / `sandbox_policy` / `journal` 为可选注入（M4 起）：
/// - `sandbox_driver` + `sandbox_policy`：`shell.run` 在 spawn 子进程前调用
///   `SandboxDriver::apply` 应用内核级沙箱（第二道防线，C-22）；
/// - `journal`：`fs.write`/`fs.edit`/`fs.delete` 成功后调 `Journal::record` 记录
///   文件改动用于 `/undo`（仅 `file-undo` feature 启用时注入，C-28）。
///
/// 未注入时这些能力退化为 no-op（兼容 M1-M3 测试）。
#[derive(Clone)]
pub struct ToolContext {
    pub workdir: Utf8PathBuf,
    pub session_id: String,
    pub canceller: CancellationToken,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    /// 单次文件读取上限（`fs.read` 用；CORE-2 接线：此前
    /// `RuntimeConfig.tools.fs_max_read_bytes` 无消费者，读取上限硬编码）。
    pub max_read_bytes: usize,
    /// OS 沙箱驱动（可选，`shell.run` 用）。
    pub sandbox_driver: Option<Arc<dyn SandboxDriver>>,
    /// OS 沙箱策略（可选，与 `sandbox_driver` 配套）。
    pub sandbox_policy: Option<SandboxPolicy>,
    /// 文件改动 journal（可选，`fs.write`/`fs.edit`/`fs.delete` 用）。
    pub journal: Option<Arc<dyn Journal>>,
    /// 点对点交互器（可选，`ui.ask` 类主动提问工具用，见 `design.md` §9.1）。
    ///
    /// Runtime 注入真实实现（Interactive/Tui/Web Prompter）；未注入时
    /// `ui.ask` 返回"用户不可达"。与权限链的 prompter 同一实例（复用
    /// `PermissionRequest` 事件广播 → 前端弹窗通道）。
    pub prompter: Option<Arc<dyn crate::policy::PermissionPrompter>>,
    /// 事件总线（可选，`ui.ask` 广播 PermissionRequested/Resolved 用——与
    /// 权限链同一 UX 通路）。未注入时 `ui.ask` 仍可走无 UI 的 prompter。
    pub events: Option<crate::runtime::EventBus>,
    /// 审计 sink（可选，PTM-12：`ui.ask` 的 Allow/Deny 决策落 `audit.log`
    /// 用——AGENTS.md §5.5 要求任何权限决策必落审计；未注入时跳过）。
    pub audit: Option<Arc<dyn crate::storage::AuditSink>>,
    /// R10-03：沙箱初始化失败时 fail-closed（`true` → 拒绝执行，不询问
    /// 沙箱外运行）。由 Runtime 从 `sandbox_fail_closed` 配置注入，供
    /// `maybe_sandbox_fallback` 判定；CI/exec 场景为 `true`。
    pub sandbox_fail_closed: bool,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("workdir", &self.workdir)
            .field("session_id", &self.session_id)
            .field("timeout", &self.timeout)
            .field("has_sandbox", &self.sandbox_driver.is_some())
            .field("has_journal", &self.journal.is_some())
            .finish_non_exhaustive()
    }
}

impl ToolContext {
    /// 创建默认上下文（无沙箱/journal 注入，兼容 M1-M3 测试）。
    ///
    /// `env` 按白名单从当前进程环境填充（C-04；2026-08-23 审查 §6-P1）：
    /// `git.*`/后台 shell 等经 `ctx.env` 下传子进程的工具不再拿到空环境。
    #[must_use]
    pub fn new(workdir: Utf8PathBuf, session_id: String) -> Self {
        Self {
            workdir,
            session_id,
            canceller: CancellationToken::new(),
            env: sanitized_env(),
            timeout: Duration::from_secs(120),
            max_output_bytes: 1024 * 1024,
            max_read_bytes: 1024 * 1024,
            sandbox_driver: None,
            sandbox_policy: None,
            journal: None,
            prompter: None,
            events: None,
            audit: None,
            sandbox_fail_closed: false,
        }
    }

    /// 链式注入执行限制（CORE-2，2026-08-25 R2 审查）：超时与输出/读取上限
    /// 来自 `RuntimeConfig.tools`——此前三字段死配置，`ToolContext` 硬编码
    /// 120s/1MiB，用户配置被静默截杀（C-07 可配承诺落空）。
    #[must_use]
    pub fn with_limits(
        mut self,
        timeout: Duration,
        max_output_bytes: usize,
        max_read_bytes: usize,
    ) -> Self {
        self.timeout = timeout;
        self.max_output_bytes = max_output_bytes;
        self.max_read_bytes = max_read_bytes;
        self
    }

    /// 链式注入协作取消令牌（CORE-3，2026-08-25 R2 审查）。
    ///
    /// 此前每次构造新建孤立 token，Runtime 的 `cancel_token` 从不下传、22 个内置
    /// 工具无一读取——协作式取消契约空转，Ctrl-C 只能靠 drop future 硬中断。
    /// 注入后长任务工具可轮询 `canceller.is_cancelled()` 优雅收尾（分批落盘/
    /// 清理子进程后再返回）。
    #[must_use]
    pub fn with_canceller(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.canceller = token;
        self
    }

    /// 链式注入点对点交互器（可选版本，便于 `Runtime` 透传）。
    #[must_use]
    pub fn with_prompter_opt(
        mut self,
        prompter: Option<Arc<dyn crate::policy::PermissionPrompter>>,
    ) -> Self {
        self.prompter = prompter;
        self
    }

    /// 链式注入事件总线（可选版本，`ui.ask` 广播事件用）。
    #[must_use]
    pub fn with_events_opt(mut self, events: Option<crate::runtime::EventBus>) -> Self {
        self.events = events;
        self
    }

    /// 链式注入审计 sink（可选版本，PTM-12：`ui.ask` 决策落 `audit.log`）。
    #[must_use]
    pub fn with_audit_opt(mut self, audit: Option<Arc<dyn crate::storage::AuditSink>>) -> Self {
        self.audit = audit;
        self
    }

    /// 链式注入沙箱驱动与策略。
    #[must_use]
    pub fn with_sandbox(mut self, driver: Arc<dyn SandboxDriver>, policy: SandboxPolicy) -> Self {
        self.sandbox_driver = Some(driver);
        self.sandbox_policy = Some(policy);
        self
    }

    /// R10-03：设置沙箱 fail-closed 标志（`true` → 沙箱初始化失败拒绝执行）。
    #[must_use]
    pub fn with_sandbox_fail_closed(mut self, fail_closed: bool) -> Self {
        self.sandbox_fail_closed = fail_closed;
        self
    }

    /// 链式注入 journal。
    #[must_use]
    pub fn with_journal(mut self, journal: Arc<dyn Journal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// 链式注入可选 journal（`Option` 版本，便于 `Runtime` 透传 `Option<Arc<...>>`）。
    #[must_use]
    pub fn with_journal_opt(mut self, journal: Option<Arc<dyn Journal>>) -> Self {
        self.journal = journal;
        self
    }
}

/// 工具 trait（可替换能力契约，`dyn` 兼容）。
///
/// 内置工具在 `minicoding-tools` 实现，MCP 工具在 `minicoding-mcp` 包装。
/// `is_read_only()` 默认基于 `side_effect()`，MCP 工具可据 `readOnlyHint` 覆盖。
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> &ToolSchema;
    fn side_effect(&self) -> SideEffect;

    /// 是否只读（用于 Plan 模式硬门，见 `design.md` §16.1）。
    /// 默认实现：`self.side_effect() == SideEffect::None`。
    /// MCP 工具根据 server schema 的 `readOnlyHint` 覆盖。
    fn is_read_only(&self) -> bool {
        self.side_effect() == SideEffect::None
    }

    /// 执行工具调用。
    fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>>;

    /// 输出 JSON Schema（R-05，M-11）：声明执行结果的结构化形态。
    ///
    /// `Some` 表示该工具返回 `ToolContent::Json`，前端可据此校验数据合法性；
    /// `None` 表示仅自由文本（默认）。只对返回 JSON 的工具提供。
    fn output_schema(&self) -> Option<&ToolOutputSchema> {
        None
    }

    /// 输出渲染意图（R-05，M-11）：把执行结果投影为结构化渲染描述。
    ///
    /// 默认实现 [`RenderIntent::default_for`]：文本直出 / JSON 美化，与 M-11 之前
    /// 的渲染行为一致（回归保底）。结构化工具（`task.*`/`plan.*`/`fs.glob` 等）
    /// 覆盖此方法提供卡片化渲染。
    fn render_output(&self, result: &ToolResult) -> RenderIntent {
        RenderIntent::default_for(result)
    }
}
