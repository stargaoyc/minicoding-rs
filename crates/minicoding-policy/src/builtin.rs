//! 内置权限策略（`BuiltinPolicy`）。
//!
//! 实现 `core::policy::PermissionPolicy`，按副作用类别产出 `Verdict`：
//! - 只读（`None`）：`Allow`；
//! - 文件写入（`FileWrite`）：校验路径落在 `workdir` 内后 `Ask`（中风险），
//!   越界直接 `Deny`（C-03）；
//! - 命令（`Command`）/网络（`Network`）：`Ask`（高风险）。
//!
//! 黑名单最高优先级（C-02）：对项目约束文件 `AGENTS.md`/`CLAUDE.md` 的
//! 破坏性删除操作硬 `Deny`，任何用户配置与 `Hook` 都无法覆盖（C-23）。
//! 对项目约束文件的写入/编辑 `Ask` 且不提供 `AllowAlways` 选项（C-23）。
//!
//! 决策逻辑本身是同步的，`check` 仅用薄 async 包装包裹已计算的 owned
//! `Verdict`，避免在 `BoxFuture` 中跨越 await 捕获引用入参（与 core 中
//! `LlmProvider::chat_stream` 取 owned 入参的 `BoxFuture` 惯例一致）。

use crate::path_sandbox::resolve_under;
use camino::Utf8Path;
use minicoding_core::model::{PolicyError, SideEffect};
use minicoding_core::policy::{
    PermissionContext, PermissionPolicy, PermissionPrompt, PromptOption, Risk, Verdict,
};
use minicoding_core::provider::BoxFuture;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

/// 全局递增的权限请求 ID 计数器（无额外依赖即可生成唯一 id）。
static PROMPT_ID: AtomicU64 = AtomicU64::new(0);

/// 内置权限策略：副作用分级 + 不可覆盖黑名单。
///
/// 见 `docs/rules.md` C-01/C-02/C-03/C-23 与 `docs/design.md` §9。
pub struct BuiltinPolicy;

impl BuiltinPolicy {
    /// 创建内置策略实例。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for BuiltinPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionPolicy for BuiltinPolicy {
    fn check(
        &self,
        tool: &str,
        input: &Value,
        ctx: &PermissionContext,
    ) -> BoxFuture<'_, Result<Verdict, PolicyError>> {
        // 同步计算 owned 判定，async 块不捕获引用入参，future 可适配任意生命周期。
        let verdict = compute_verdict(tool, input, ctx);
        Box::pin(async move { Ok(verdict) })
    }
}

/// 同步决策核心：按副作用分级 + 黑名单产出 [`Verdict`]。
fn compute_verdict(tool: &str, input: &Value, ctx: &PermissionContext) -> Verdict {
    // 黑名单最高优先级（C-02）：在一切用户配置与 Hook 之前生效。
    if is_blacklisted(tool, input) {
        return Verdict::Deny(format!(
            "destructive op on project doc is blacklisted: {tool}"
        ));
    }
    match ctx.side_effect {
        SideEffect::None => Verdict::Allow,
        SideEffect::FileWrite => check_file_write(tool, input, ctx),
        SideEffect::Command => Verdict::Ask(make_prompt(
            tool,
            command_summary(input),
            Risk::High,
            full_options(),
        )),
        SideEffect::Network => Verdict::Ask(make_prompt(
            tool,
            network_summary(input),
            Risk::High,
            full_options(),
        )),
    }
}

/// 判定是否命中内置黑名单（C-02/C-23）。
///
/// 对项目约束文件的破坏性删除操作硬 `Deny`，配置无法覆盖。
fn is_blacklisted(tool: &str, input: &Value) -> bool {
    if tool != "fs.delete" {
        return false;
    }
    extract_path(input).is_some_and(targets_project_doc)
}

/// 文件写入类工具的权限判定。
fn check_file_write(tool: &str, input: &Value, ctx: &PermissionContext) -> Verdict {
    let Some(path) = extract_path(input) else {
        // 无 path 字段：仍需询问，由工具自身做最终校验。
        return Verdict::Ask(make_prompt(
            tool,
            "写入文件".to_string(),
            Risk::Medium,
            full_options(),
        ));
    };

    if targets_project_doc(path) {
        // C-23：项目约束文件写入不可 AllowAlways。
        return Verdict::Ask(make_prompt(
            tool,
            format!("写入项目约束文件 {path}"),
            Risk::Medium,
            project_doc_options(),
        ));
    }

    // C-03：写入路径必须落在 workdir 之内，否则直接 Deny。
    match resolve_under(&ctx.workdir, path) {
        Ok(_) => Verdict::Ask(make_prompt(
            tool,
            format!("写入文件 {path}"),
            Risk::Medium,
            full_options(),
        )),
        Err(e) => Verdict::Deny(format!("path not allowed: {e}")),
    }
}

/// 判定路径是否指向项目约束文件（`AGENTS.md`/`CLAUDE.md`）。
fn targets_project_doc(path: &str) -> bool {
    match Utf8Path::new(path).file_name() {
        Some(name) => name == "AGENTS.md" || name == "CLAUDE.md",
        None => false,
    }
}

/// 从工具输入 JSON 中提取 `path` 字段。
fn extract_path(input: &Value) -> Option<&str> {
    input.get("path")?.as_str()
}

/// 构造命令执行风险摘要。
fn command_summary(input: &Value) -> String {
    let cmd = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(|v| v.as_str());
    match cmd {
        Some(c) => format!("执行命令 {c}"),
        None => "执行命令".to_string(),
    }
}

/// 构造网络访问风险摘要。
fn network_summary(input: &Value) -> String {
    let url = input.get("url").and_then(|v| v.as_str());
    match url {
        Some(u) => format!("访问网络 {u}"),
        None => "访问网络".to_string(),
    }
}

/// 常规询问选项（含 `AllowAlways`）。
fn full_options() -> Vec<PromptOption> {
    vec![
        PromptOption::AllowOnce,
        PromptOption::AllowAlways,
        PromptOption::DenyOnce,
        PromptOption::DenyAlways,
    ]
}

/// 项目约束文件询问选项（C-23：不含 `AllowAlways`）。
fn project_doc_options() -> Vec<PromptOption> {
    vec![
        PromptOption::AllowOnce,
        PromptOption::DenyOnce,
        PromptOption::DenyAlways,
    ]
}

/// 生成下一个唯一权限请求 id。
fn next_prompt_id() -> String {
    format!("prompt-{}", PROMPT_ID.fetch_add(1, Ordering::Relaxed))
}

/// 组装 [`PermissionPrompt`]。
fn make_prompt(
    tool: &str,
    summary: String,
    risk: Risk,
    options: Vec<PromptOption>,
) -> PermissionPrompt {
    PermissionPrompt {
        id: next_prompt_id(),
        tool: tool.to_string(),
        summary,
        risk,
        options,
    }
}
