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
//! Plan 模式硬门（C-25，`design.md` §16.1）：`PermissionMode::Plan` 下所有
//! `side_effect != None` 工具直接 `Deny("plan mode: read-only")`。这是 L0
//! 扩展（黑名单之后的最高优先级），不可被 L1 用户策略/Hook 覆盖。声明了
//! `readOnlyHint` 的 MCP 工具 `side_effect == None` 不受影响（C-25 留通道）。
//!
//! 预批准缓存（`design.md` §16.4）：`plan.exit` 缓存的 `allowed_prompts` 命中
//! 时直接 `Allow`，跳过 `Ask` 与 prompter（用户在批准 plan 时已一次性授权）。
//!
//! 决策逻辑本身是同步的，`check` 仅用薄 async 包装包裹已计算的 owned
//! `Verdict`，避免在 `BoxFuture` 中跨越 await 捕获引用入参（与 core 中
//! `LlmProvider::chat_stream` 取 owned 入参的 `BoxFuture` 惯例一致）。

use crate::path_sandbox::resolve_under;
use camino::Utf8Path;
use minicoding_core::model::{PolicyError, SideEffect};
use minicoding_core::policy::{
    PermissionContext, PermissionMode, PermissionPolicy, PermissionPrompt, PreApprovedPrompt,
    PromptOption, Risk, Verdict,
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
    // Plan 模式硬门（C-25，design.md §16.1）：Plan 模式下所有 side_effect != None
    // 工具直接 Deny。L0 扩展，不可被 L1/Hook 覆盖。`is_read_only()` 由 Tool trait
    // 默认 `side_effect == None` 判定（MCP 工具可据 readOnlyHint 覆盖），此处用
    // `side_effect != None` 同义判定（policy 不依赖 tools crate）。
    if ctx.permission_mode == PermissionMode::Plan && ctx.side_effect != SideEffect::None {
        return Verdict::Deny("plan mode: read-only".to_string());
    }
    // 预批准缓存命中（design.md §16.4）：tool 与 prompt 子串匹配时直接 Allow，
    // 跳过 Ask 与 prompter。这是 plan.exit 后用户一次性授权的便利点。
    if ctx.side_effect != SideEffect::None
        && matches_pre_approved(tool, input, &ctx.allowed_prompts)
    {
        return Verdict::Allow;
    }
    // memory.write 特殊路由（C-23/C-27）：按 target 与内容模式细分。
    if tool == "memory.write" {
        return check_memory_write(tool, input);
    }
    match ctx.side_effect {
        SideEffect::None => Verdict::Allow,
        SideEffect::FileWrite => check_file_write(tool, input, ctx),
        // BypassPermissions（design.md §16.2）：全放行（仅隔离容器内使用，对齐 CC
        // `bypassPermissions`）。文件写入仍走 `check_file_write` 保留 C-03 越界 Deny
        // 与 C-23 项目约束文件 Ask——L0 硬约束不受用户模式影响。
        SideEffect::Command | SideEffect::Network
            if ctx.permission_mode == PermissionMode::BypassPermissions =>
        {
            Verdict::Allow
        }
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

/// 检测工具调用是否命中 `plan.exit` 缓存的预批准清单（S6 词法比对版）。
///
/// `tool` 完全相等且命令满足以下之一：
/// - 与预批准 prompt **词法完全相等**；
/// - 以 prompt 为**词边界前缀**（`cargo build` 批准 `cargo build --release`）；
/// - 命令含复合操作符（`;`/`&&`/`||`/`|`/反引号/`$(`）时**永不命中**——复合命令
///   不继承预批准（防 `git push; echo cargo test` 拼接绕过）。
fn matches_pre_approved(tool: &str, input: &Value, allowed: &[PreApprovedPrompt]) -> bool {
    if allowed.is_empty() {
        return false;
    }
    let Some(command_text) = extract_command_text(input) else {
        return false;
    };
    // 复合命令一律不继承预批准（S6）
    if [";", "&&", "||", "`", "$(", "|"]
        .iter()
        .any(|op| command_text.contains(op))
    {
        return false;
    }
    let cmd_tokens = tokenize_command(&command_text);
    allowed.iter().any(|p| {
        if p.tool != tool || p.prompt.is_empty() {
            return false;
        }
        let prompt_tokens = tokenize_command(&p.prompt);
        if prompt_tokens.is_empty() {
            return false;
        }
        // 完全相等 或 词边界前缀
        cmd_tokens.len() >= prompt_tokens.len()
            && cmd_tokens[..prompt_tokens.len()] == prompt_tokens[..]
    })
}

/// 从工具输入中提取"命令文本"用于预批准匹配。
///
/// `shell.run` 取 `command` 或 `cmd` 字段；其它工具无标准命令字段时返回 `None`
/// （不参与预批准，保守拒绝匹配）。
fn extract_command_text(input: &Value) -> Option<String> {
    input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// `memory.write` 权限路由（C-23/C-27）。
///
/// - `target: "long_term"` → `Ask`（C-23：手写长期记忆写入需用户确认）；
/// - `target: "auto"` → 默认 `Allow`（隐式自动学习），但内容含指令性模式时
///   降级 `Ask`（C-27：Auto memory 不可作为越权通道）。
fn check_memory_write(tool: &str, input: &Value) -> Verdict {
    let target = input.get("target").and_then(|v| v.as_str());
    let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
    match target {
        Some("long_term") => Verdict::Ask(make_prompt(
            tool,
            "写入长期记忆（全量覆盖 long_term.md）".to_string(),
            Risk::Medium,
            full_options(),
        )),
        Some("auto") => {
            if is_instructional_content(content) {
                Verdict::Ask(make_prompt(
                    tool,
                    "写入 Auto memory：内容含指令性模式，需确认（C-27）".to_string(),
                    Risk::Medium,
                    full_options(),
                ))
            } else {
                Verdict::Allow
            }
        }
        _ => Verdict::Ask(make_prompt(
            tool,
            "写入记忆（未知 target）".to_string(),
            Risk::Medium,
            full_options(),
        )),
    }
}

/// 检测内容是否含指令性模式（C-27 降级用）。
///
/// 与 `minicoding-memory::auto::is_instructional` 同语义：检测祈使/模态/规则头。
/// 此处独立实现避免 `minicoding-policy` 依赖 `minicoding-memory`（领域 crate 不交叉）。
fn is_instructional_content(content: &str) -> bool {
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("always use")
            || lower.starts_with("never ")
            || lower.starts_with("must ")
            || lower.starts_with("do not ")
            || lower.starts_with("don't ")
            || lower.starts_with("should ")
        {
            return true;
        }
        if line.starts_with("总是")
            || line.starts_with("永远")
            || line.starts_with("禁止")
            || line.starts_with("必须")
            || line.starts_with("不要")
            || line.starts_with("不得")
            || line.starts_with("应当")
            || line.starts_with("应")
        {
            return true;
        }
        if lower.starts_with("## rules")
            || lower.starts_with("## constraints")
            || line.starts_with("## 规则")
            || line.starts_with("## 约束")
        {
            return true;
        }
    }
    false
}

/// 判定是否命中内置黑名单（C-02/C-23）。
///
/// 对项目约束文件的破坏性删除操作硬 `Deny`，配置无法覆盖。
fn is_blacklisted(tool: &str, input: &Value) -> bool {
    match tool {
        // C-23：项目约束文件——fs.delete 硬 Deny；fs.write 走 Ask+不可 AllowAlways
        // 通道（check_file_write，允许一次性人工批准），shell 旁路由 shell_hits_blacklist 补齐
        "fs.delete" => {
            extract_path(input).is_some_and(|p| targets_project_doc(p) || in_vcs_metadata(p))
        }
        // S5：VCS 元数据写入（.git/hooks/pre-commit 植入等）无合法用例，硬 Deny
        "fs.write" | "fs.edit" => extract_path(input).is_some_and(in_vcs_metadata),
        // S5：shell 旁路——写约束文件 / 写 VCS 元数据（rm AGENTS.md、> .git/hooks/x 等）
        "shell.run" => shell_hits_blacklist(input),
        _ => false,
    }
}

/// S5/C-23：shell.run 命令是否以受保护目标为**写对象**。
///
/// 词法近似判定（诚实边界：base64|sh 等变形不在黑名单能力内，由沙箱与用户审批兜底）：
/// - 破坏性动词（`rm`/`mv` 第一目的/`truncate`/`dd`/`sed -i`/`unlink`）后随
///   `AGENTS.md`/`CLAUDE.md` 路径；
/// - 重定向（`>`/`>>`）或 `tee` 目标为约束文件；
/// - 任一 token 路径组件命中 VCS 元数据目录且伴随写意图（重定向/tee/`.git/hooks`）。
fn shell_hits_blacklist(input: &Value) -> bool {
    // 写意图动词：`sed` 需搭配 `-i` 才是写；`tee` 本身即写
    const WRITE_VERBS: &[&str] = &["rm", "mv", "truncate", "dd", "unlink", "sed", "tee"];
    const REDIRECTS: &[&str] = &[">", ">>", "&>", ">|"];
    let Some(cmd) = extract_command_text(input) else {
        return false;
    };

    // 按命令分隔符切段逐段独立判定（`;`/`|`/反引号/`$()`——`&&`/`||` 含于 `&`/`|`
    // 的字符级切分；粗粒度切分只会影响检测灵敏度，方向 fail-closed）。
    cmd.split([';', '|', '&', '`'])
        .map(str::trim)
        .filter(|seg| !seg.is_empty() && *seg != "$(")
        .any(|segment| {
            let tokens = tokenize_command(segment);
            if tokens.is_empty() {
                return false;
            }
            // 段首词即动词（shell 语法保证）；`sed -i` 特判写模式
            let verb_writes = WRITE_VERBS.contains(&tokens[0].as_str())
                && (tokens[0] != "sed" || tokens.iter().any(|t| t == "-i"));
            tokens.iter().enumerate().any(|(i, tok)| {
                if !(targets_project_doc(tok) || in_vcs_metadata(tok)) {
                    return false;
                }
                // 重定向目标：紧邻前一个 token 是重定向符
                let redirect_target = i > 0 && REDIRECTS.contains(&tokens[i - 1].as_str());
                verb_writes || redirect_target
            })
        })
}

/// S5：命令词法切分——空白切分 + 剥离引号包裹 + 处理 `cmd>=file` 连写形态。
fn tokenize_command(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                current.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            // 连写重定向：`x>y` 拆为 ["x", ">", "y"]
            '>' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                let next = chars.peek().copied();
                if next == Some('>') || next == Some('&') || next == Some('|') {
                    let mut op = String::from(">");
                    op.push(chars.next().expect("peeked"));
                    tokens.push(op);
                } else {
                    tokens.push(">".into());
                }
            }
            _ if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// S5：路径是否位于 VCS 元数据目录内（.git/.hg/.svn 任一组件）。
fn in_vcs_metadata(path: &str) -> bool {
    Utf8Path::new(path)
        .components()
        .any(|c| matches!(c.as_str(), ".git" | ".hg" | ".svn"))
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
        Ok(_) => {
            // `AcceptEdits`/`BypassPermissions` 模式（design.md §16.2）：工作区内文件
            // 编辑自动 Allow，不弹窗（高频编辑场景）；shell/网络仍 Ask（危险操作需
            // 确认，BypassPermissions 除外）。项目约束文件（C-23）与越界路径（C-03）
            // 已在上面分支拦截，不进入此处。
            if matches!(
                ctx.permission_mode,
                PermissionMode::AcceptEdits | PermissionMode::BypassPermissions
            ) {
                return Verdict::Allow;
            }
            Verdict::Ask(make_prompt(
                tool,
                format!("写入文件 {path}"),
                Risk::Medium,
                full_options(),
            ))
        }
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

#[cfg(test)]
mod tests {
    //! `memory.write` 权限路由测试（C-23/C-27）+ Plan 模式硬门（C-25）
    //! + 预批准缓存（design.md §16.4）测试。

    use super::*;
    use camino::Utf8PathBuf;
    use minicoding_core::model::SideEffect;
    use minicoding_core::policy::{PermissionContext, PermissionMode};

    fn ctx_file_write() -> PermissionContext {
        PermissionContext {
            session: "test".to_string(),
            workdir: Utf8PathBuf::from("/tmp/proj"),
            side_effect: SideEffect::FileWrite,
            turn: 0,
            history: Vec::new(),
            permission_mode: PermissionMode::Default,
            allowed_prompts: Vec::new(),
        }
    }

    fn ctx_with_mode(side_effect: SideEffect, mode: PermissionMode) -> PermissionContext {
        PermissionContext {
            session: "test".to_string(),
            workdir: Utf8PathBuf::from("/tmp/proj"),
            side_effect,
            turn: 0,
            history: Vec::new(),
            permission_mode: mode,
            allowed_prompts: Vec::new(),
        }
    }

    fn ctx_with_allowed(
        side_effect: SideEffect,
        allowed: Vec<PreApprovedPrompt>,
    ) -> PermissionContext {
        PermissionContext {
            session: "test".to_string(),
            workdir: Utf8PathBuf::from("/tmp/proj"),
            side_effect,
            turn: 0,
            history: Vec::new(),
            permission_mode: PermissionMode::Default,
            allowed_prompts: allowed,
        }
    }

    #[tokio::test]
    async fn memory_write_long_term_returns_ask() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"target": "long_term", "content": "remember this"});
        let verdict = policy
            .check("memory.write", &input, &ctx_file_write())
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[tokio::test]
    async fn memory_write_auto_non_instructional_returns_allow() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({
            "target": "auto",
            "content": "user prefers 4-space indent",
            "topic": "indent"
        });
        let verdict = policy
            .check("memory.write", &input, &ctx_file_write())
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Allow));
    }

    #[tokio::test]
    async fn memory_write_auto_instructional_english_returns_ask() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({
            "target": "auto",
            "content": "Always use cargo fmt before commit"
        });
        let verdict = policy
            .check("memory.write", &input, &ctx_file_write())
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[tokio::test]
    async fn memory_write_auto_instructional_chinese_returns_ask() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({
            "target": "auto",
            "content": "禁止提交密钥到仓库"
        });
        let verdict = policy
            .check("memory.write", &input, &ctx_file_write())
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[tokio::test]
    async fn memory_write_auto_section_header_returns_ask() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({
            "target": "auto",
            "content": "## Rules\n- rule 1\n- rule 2"
        });
        let verdict = policy
            .check("memory.write", &input, &ctx_file_write())
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[tokio::test]
    async fn memory_write_unknown_target_returns_ask() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"target": "unknown", "content": "x"});
        let verdict = policy
            .check("memory.write", &input, &ctx_file_write())
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[tokio::test]
    async fn memory_write_missing_target_returns_ask() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"content": "x"});
        let verdict = policy
            .check("memory.write", &input, &ctx_file_write())
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[test]
    fn is_instructional_content_negative_descriptive() {
        assert!(!is_instructional_content("user prefers dark theme"));
        assert!(!is_instructional_content("the project uses rust 2024"));
        assert!(!is_instructional_content(""));
    }

    // Plan 模式硬门测试（C-25，design.md §16.1）

    #[tokio::test]
    async fn plan_mode_denies_file_write() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "src/main.rs", "content": "x"});
        let verdict = policy
            .check(
                "fs.write",
                &input,
                &ctx_with_mode(SideEffect::FileWrite, PermissionMode::Plan),
            )
            .await
            .unwrap();
        match verdict {
            Verdict::Deny(msg) => assert!(msg.contains("plan mode")),
            other => panic!("期望 Deny，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_mode_denies_shell_run() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"command": "cargo build"});
        let verdict = policy
            .check(
                "shell.run",
                &input,
                &ctx_with_mode(SideEffect::Command, PermissionMode::Plan),
            )
            .await
            .unwrap();
        match verdict {
            Verdict::Deny(msg) => assert!(msg.contains("plan mode")),
            other => panic!("期望 Deny，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn plan_mode_allows_readonly_tools() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "src/main.rs"});
        let verdict = policy
            .check(
                "fs.read",
                &input,
                &ctx_with_mode(SideEffect::None, PermissionMode::Plan),
            )
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Allow));
    }

    #[tokio::test]
    async fn plan_mode_blacklist_still_highest() {
        // 黑名单 C-02 优先级高于 Plan 硬门：fs.delete AGENTS.md 应返回黑名单 Deny
        // （消息含 "blacklisted"），而非 "plan mode"
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "AGENTS.md"});
        let verdict = policy
            .check(
                "fs.delete",
                &input,
                &ctx_with_mode(SideEffect::FileWrite, PermissionMode::Plan),
            )
            .await
            .unwrap();
        match verdict {
            Verdict::Deny(msg) => assert!(msg.contains("blacklisted")),
            other => panic!("期望黑名单 Deny，实际 {other:?}"),
        }
    }

    // 预批准缓存测试（design.md §16.4）

    #[tokio::test]
    async fn pre_approved_shell_run_matches_prefix() {
        let policy = BuiltinPolicy::new();
        let allowed = vec![PreApprovedPrompt {
            tool: "shell.run".to_string(),
            prompt: "cargo build".to_string(),
        }];
        // "cargo build --release" 包含 "cargo build" → Allow
        let input = serde_json::json!({"command": "cargo build --release"});
        let verdict = policy
            .check(
                "shell.run",
                &input,
                &ctx_with_allowed(SideEffect::Command, allowed),
            )
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Allow));
    }

    #[tokio::test]
    async fn pre_approved_mismatch_returns_ask() {
        let policy = BuiltinPolicy::new();
        let allowed = vec![PreApprovedPrompt {
            tool: "shell.run".to_string(),
            prompt: "cargo build".to_string(),
        }];
        // "cargo test" 不包含 "cargo build" → Ask
        let input = serde_json::json!({"command": "cargo test"});
        let verdict = policy
            .check(
                "shell.run",
                &input,
                &ctx_with_allowed(SideEffect::Command, allowed),
            )
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[tokio::test]
    async fn pre_approved_wrong_tool_does_not_match() {
        let policy = BuiltinPolicy::new();
        let allowed = vec![PreApprovedPrompt {
            tool: "shell.run".to_string(),
            prompt: "cargo build".to_string(),
        }];
        // tool 不匹配 → Ask
        let input = serde_json::json!({"command": "cargo build"});
        let verdict = policy
            .check(
                "shell.run.other",
                &input,
                &ctx_with_allowed(SideEffect::Command, allowed),
            )
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[tokio::test]
    async fn pre_approved_empty_prompt_does_not_match() {
        let policy = BuiltinPolicy::new();
        let allowed = vec![PreApprovedPrompt {
            tool: "shell.run".to_string(),
            prompt: String::new(),
        }];
        let input = serde_json::json!({"command": "cargo build"});
        let verdict = policy
            .check(
                "shell.run",
                &input,
                &ctx_with_allowed(SideEffect::Command, allowed),
            )
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    // === SideEffect 分支补充测试（Default 模式下 None/Command/Network）===

    #[tokio::test]
    async fn side_effect_none_returns_allow_in_default_mode() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "src/main.rs"});
        let verdict = policy
            .check(
                "fs.read",
                &input,
                &ctx_with_mode(SideEffect::None, PermissionMode::Default),
            )
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Allow));
    }

    #[tokio::test]
    async fn side_effect_command_returns_ask_high_risk_with_full_options() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"command": "ls -la"});
        let verdict = policy
            .check(
                "shell.run",
                &input,
                &ctx_with_mode(SideEffect::Command, PermissionMode::Default),
            )
            .await
            .unwrap();
        match verdict {
            Verdict::Ask(prompt) => {
                assert_eq!(prompt.risk, Risk::High);
                assert!(prompt.summary.contains("ls -la"));
                assert!(prompt.options.contains(&PromptOption::AllowAlways));
                assert!(prompt.options.contains(&PromptOption::DenyAlways));
            }
            other => panic!("期望 Ask，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn side_effect_command_without_command_field_returns_ask() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({});
        let verdict = policy
            .check(
                "shell.run",
                &input,
                &ctx_with_mode(SideEffect::Command, PermissionMode::Default),
            )
            .await
            .unwrap();
        match verdict {
            Verdict::Ask(prompt) => assert_eq!(prompt.summary, "执行命令"),
            other => panic!("期望 Ask，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn side_effect_network_returns_ask_high_risk() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"url": "https://example.com"});
        let verdict = policy
            .check(
                "web.fetch",
                &input,
                &ctx_with_mode(SideEffect::Network, PermissionMode::Default),
            )
            .await
            .unwrap();
        match verdict {
            Verdict::Ask(prompt) => {
                assert_eq!(prompt.risk, Risk::High);
                assert!(prompt.summary.contains("https://example.com"));
            }
            other => panic!("期望 Ask，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn side_effect_network_without_url_returns_ask() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({});
        let verdict = policy
            .check(
                "web.fetch",
                &input,
                &ctx_with_mode(SideEffect::Network, PermissionMode::Default),
            )
            .await
            .unwrap();
        match verdict {
            Verdict::Ask(prompt) => assert_eq!(prompt.summary, "访问网络"),
            other => panic!("期望 Ask，实际 {other:?}"),
        }
    }

    // === check_file_write 分支测试 ===

    #[tokio::test]
    async fn file_write_no_path_returns_ask_with_full_options() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"content": "x"});
        let verdict = policy
            .check("fs.write", &input, &ctx_file_write())
            .await
            .unwrap();
        match verdict {
            Verdict::Ask(prompt) => {
                assert_eq!(prompt.summary, "写入文件");
                assert_eq!(prompt.risk, Risk::Medium);
                assert!(prompt.options.contains(&PromptOption::AllowAlways));
            }
            other => panic!("期望 Ask，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_write_agents_md_returns_ask_without_allow_always() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "AGENTS.md", "content": "x"});
        let verdict = policy
            .check("fs.write", &input, &ctx_file_write())
            .await
            .unwrap();
        match verdict {
            Verdict::Ask(prompt) => {
                assert!(prompt.summary.contains("AGENTS.md"));
                assert!(!prompt.options.contains(&PromptOption::AllowAlways));
                assert_eq!(prompt.options.len(), 3);
            }
            other => panic!("期望 Ask，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_write_claude_md_returns_ask_without_allow_always() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "CLAUDE.md", "content": "x"});
        let verdict = policy
            .check("fs.write", &input, &ctx_file_write())
            .await
            .unwrap();
        match verdict {
            Verdict::Ask(prompt) => assert!(!prompt.options.contains(&PromptOption::AllowAlways)),
            other => panic!("期望 Ask，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_write_in_workdir_returns_ask() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workdir =
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("tempdir path is utf8");
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"});
        let ctx = PermissionContext {
            session: "test".to_string(),
            workdir,
            side_effect: SideEffect::FileWrite,
            turn: 0,
            history: Vec::new(),
            permission_mode: PermissionMode::Default,
            allowed_prompts: Vec::new(),
        };
        let verdict = policy.check("fs.write", &input, &ctx).await.unwrap();
        match verdict {
            Verdict::Ask(prompt) => {
                assert!(prompt.summary.contains("src/main.rs"));
                assert!(prompt.options.contains(&PromptOption::AllowAlways));
            }
            other => panic!("期望 Ask，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_write_accept_edits_mode_returns_allow() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workdir =
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("tempdir path is utf8");
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"});
        let ctx = PermissionContext {
            session: "test".to_string(),
            workdir,
            side_effect: SideEffect::FileWrite,
            turn: 0,
            history: Vec::new(),
            permission_mode: PermissionMode::AcceptEdits,
            allowed_prompts: Vec::new(),
        };
        let verdict = policy.check("fs.write", &input, &ctx).await.unwrap();
        assert!(
            matches!(verdict, Verdict::Allow),
            "AcceptEdits 工作区内写入应 Allow，实际 {verdict:?}"
        );
    }

    #[tokio::test]
    async fn file_write_accept_edits_agents_md_still_asks() {
        // C-23：AcceptEdits 也不放行项目约束文件写入
        let tmp = tempfile::tempdir().expect("tempdir");
        let workdir =
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("tempdir path is utf8");
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "AGENTS.md", "content": "x"});
        let ctx = PermissionContext {
            session: "test".to_string(),
            workdir,
            side_effect: SideEffect::FileWrite,
            turn: 0,
            history: Vec::new(),
            permission_mode: PermissionMode::AcceptEdits,
            allowed_prompts: Vec::new(),
        };
        let verdict = policy.check("fs.write", &input, &ctx).await.unwrap();
        assert!(
            matches!(verdict, Verdict::Ask(_)),
            "AGENTS.md 应保持 Ask，实际 {verdict:?}"
        );
    }

    #[tokio::test]
    async fn file_write_accept_edits_path_escape_still_denies() {
        // C-03：AcceptEdits 也不放行越界路径
        let tmp = tempfile::tempdir().expect("tempdir");
        let workdir =
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("tempdir path is utf8");
        let policy = BuiltinPolicy::new();
        let escape_path = if cfg!(unix) {
            "../../etc/passwd"
        } else {
            "../../Windows/System32/drivers/etc/hosts"
        };
        let input = serde_json::json!({"path": escape_path, "content": "x"});
        let ctx = PermissionContext {
            session: "test".to_string(),
            workdir,
            side_effect: SideEffect::FileWrite,
            turn: 0,
            history: Vec::new(),
            permission_mode: PermissionMode::AcceptEdits,
            allowed_prompts: Vec::new(),
        };
        let verdict = policy.check("fs.write", &input, &ctx).await.unwrap();
        assert!(
            matches!(verdict, Verdict::Deny(_)),
            "越界应 Deny，实际 {verdict:?}"
        );
    }

    #[tokio::test]
    async fn file_write_path_escape_returns_deny() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workdir =
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("tempdir path is utf8");
        let policy = BuiltinPolicy::new();
        // 越界路径：相对 ../../ 穿出到 workdir 之外（C-03）
        let escape_path = if cfg!(unix) {
            "../../etc/passwd"
        } else {
            "../../Windows/System32/drivers/etc/hosts"
        };
        let input = serde_json::json!({"path": escape_path, "content": "x"});
        let ctx = PermissionContext {
            session: "test".to_string(),
            workdir,
            side_effect: SideEffect::FileWrite,
            turn: 0,
            history: Vec::new(),
            permission_mode: PermissionMode::Default,
            allowed_prompts: Vec::new(),
        };
        let verdict = policy.check("fs.write", &input, &ctx).await.unwrap();
        match verdict {
            Verdict::Deny(msg) => assert!(msg.contains("path not allowed")),
            other => panic!("期望 Deny，实际 {other:?}"),
        }
    }

    // === 黑名单（is_blacklisted）补充测试 ===

    #[tokio::test]
    async fn blacklist_denies_delete_claude_md() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "CLAUDE.md"});
        let verdict = policy
            .check("fs.delete", &input, &ctx_file_write())
            .await
            .unwrap();
        match verdict {
            Verdict::Deny(msg) => assert!(msg.contains("blacklisted")),
            other => panic!("期望黑名单 Deny，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn blacklist_does_not_match_fs_write_on_agents_md() {
        // fs.write AGENTS.md 不触发黑名单（仅 fs.delete 触发），走 check_file_write → Ask
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "AGENTS.md", "content": "x"});
        let verdict = policy
            .check("fs.write", &input, &ctx_file_write())
            .await
            .unwrap();
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[tokio::test]
    async fn blacklist_delete_other_file_in_workdir_returns_ask() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workdir =
            Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("tempdir path is utf8");
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({"path": "src/main.rs"});
        let ctx = PermissionContext {
            session: "test".to_string(),
            workdir,
            side_effect: SideEffect::FileWrite,
            turn: 0,
            history: Vec::new(),
            permission_mode: PermissionMode::Default,
            allowed_prompts: Vec::new(),
        };
        let verdict = policy.check("fs.delete", &input, &ctx).await.unwrap();
        // fs.delete 非 AGENTS.md/CLAUDE.md → 不触发黑名单 → 走 check_file_write → Ask
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[tokio::test]
    async fn blacklist_delete_without_path_returns_ask() {
        let policy = BuiltinPolicy::new();
        let input = serde_json::json!({});
        let verdict = policy
            .check("fs.delete", &input, &ctx_file_write())
            .await
            .unwrap();
        // 无 path → is_blacklisted 返回 false → 走 check_file_write → Ask（"写入文件"）
        match verdict {
            Verdict::Ask(prompt) => assert_eq!(prompt.summary, "写入文件"),
            other => panic!("期望 Ask，实际 {other:?}"),
        }
    }

    // === targets_project_doc 单元测试 ===

    // ===== S5：shell 旁路黑名单 =====

    #[test]
    fn shell_write_to_project_doc_denied() {
        for cmd in [
            "rm AGENTS.md",
            "rm -rf subdir/AGENTS.md",
            "mv CLAUDE.md /tmp/x",
            "echo injected > AGENTS.md",
            "echo x >> ./CLAUDE.md",
            "cat evil.txt | tee AGENTS.md",
            "sed -i s/a/b/ AGENTS.md",
            "truncate -s 0 CLAUDE.md",
            "echo injected>AGENTS.md",
        ] {
            let input = serde_json::json!({ "command": cmd });
            assert!(is_blacklisted("shell.run", &input), "{cmd} 应命中黑名单");
        }
    }

    #[test]
    fn shell_read_of_project_doc_allowed() {
        for cmd in ["cat AGENTS.md", "head -5 CLAUDE.md", "grep foo AGENTS.md"] {
            let input = serde_json::json!({ "command": cmd });
            assert!(!is_blacklisted("shell.run", &input), "{cmd} 读操作不应拦截");
        }
    }

    #[test]
    fn shell_vcs_metadata_write_denied() {
        for cmd in [
            "echo hook > .git/hooks/pre-commit",
            "tee .git/config < payload",
            "rm -rf .git",
        ] {
            let input = serde_json::json!({ "command": cmd });
            assert!(is_blacklisted("shell.run", &input), "{cmd} 应命中黑名单");
        }
    }

    #[test]
    fn fs_tools_vcs_metadata_denied() {
        let write = serde_json::json!({ "path": ".git/hooks/pre-commit" });
        assert!(is_blacklisted("fs.write", &write));
        let del = serde_json::json!({ "path": "subdir/.hg/hgrc" });
        assert!(is_blacklisted("fs.delete", &del) || is_blacklisted("fs.write", &del));
        // 普通路径不受限
        let normal = serde_json::json!({ "path": "src/main.rs" });
        assert!(!is_blacklisted("fs.write", &normal));
    }

    #[test]
    fn tokenize_command_handles_quotes_and_glued_redirect() {
        assert_eq!(
            tokenize_command("echo \"a > b\" > AGENTS.md"),
            vec!["echo", "a > b", ">", "AGENTS.md"]
        );
        assert_eq!(tokenize_command("x>AGENTS.md"), vec!["x", ">", "AGENTS.md"]);
        assert_eq!(tokenize_command("a >> b"), vec!["a", ">>", "b"]);
    }

    // ===== S6：预批准词法比对 =====

    fn pre(tool: &str, prompt: &str) -> PreApprovedPrompt {
        PreApprovedPrompt {
            tool: tool.into(),
            prompt: prompt.into(),
        }
    }

    #[test]
    fn pre_approved_word_prefix_matches() {
        let allowed = vec![pre("shell.run", "cargo test")];
        let hit = serde_json::json!({ "command": "cargo test --nocapture" });
        assert!(matches_pre_approved("shell.run", &hit, &allowed));
        let exact = serde_json::json!({ "command": "cargo test" });
        assert!(matches_pre_approved("shell.run", &exact, &allowed));
    }

    #[test]
    fn pre_approved_concat_bypass_blocked() {
        let allowed = vec![pre("shell.run", "cargo test")];
        for cmd in [
            "git push; echo cargo test",
            "git push && cargo test",
            "echo cargo test",
            "cargo build # cargo test",
            "$(echo cargo test)",
        ] {
            let input = serde_json::json!({ "command": cmd });
            assert!(
                !matches_pre_approved("shell.run", &input, &allowed),
                "{cmd} 不应命中预批准"
            );
        }
    }

    #[test]
    fn pre_approved_non_prefix_mismatch_rejected() {
        let allowed = vec![pre("shell.run", "cargo test")];
        // 非前缀：词序不同
        let input = serde_json::json!({ "command": "npm run cargo test" });
        assert!(!matches_pre_approved("shell.run", &input, &allowed));
    }

    #[test]
    fn targets_project_doc_matches_agents_md_variants() {
        assert!(targets_project_doc("AGENTS.md"));
        assert!(targets_project_doc("subdir/AGENTS.md"));
        assert!(targets_project_doc("./AGENTS.md"));
        assert!(targets_project_doc("/abs/path/AGENTS.md"));
    }

    #[test]
    fn targets_project_doc_matches_claude_md_variants() {
        assert!(targets_project_doc("CLAUDE.md"));
        assert!(targets_project_doc("docs/CLAUDE.md"));
    }

    #[test]
    fn targets_project_doc_rejects_other_files() {
        assert!(!targets_project_doc("README.md"));
        assert!(!targets_project_doc("agents.md")); // 大小写敏感
        assert!(!targets_project_doc("AGENTS.txt"));
        assert!(!targets_project_doc("src/main.rs"));
    }

    #[test]
    fn targets_project_doc_rejects_no_filename() {
        assert!(!targets_project_doc(""));
        assert!(!targets_project_doc("/"));
        assert!(!targets_project_doc("."));
    }

    // === extract_path / extract_command_text ===

    #[test]
    fn extract_path_returns_string_for_path_field() {
        assert_eq!(
            extract_path(&serde_json::json!({"path": "foo/bar"})),
            Some("foo/bar")
        );
    }

    #[test]
    fn extract_path_returns_none_when_absent_or_non_string() {
        assert_eq!(extract_path(&serde_json::json!({"content": "x"})), None);
        assert_eq!(extract_path(&serde_json::json!({})), None);
        assert_eq!(extract_path(&serde_json::json!({"path": 123})), None);
    }

    #[test]
    fn extract_command_text_prefers_command_field() {
        assert_eq!(
            extract_command_text(&serde_json::json!({"command": "cargo build"})),
            Some("cargo build".to_string())
        );
    }

    #[test]
    fn extract_command_text_falls_back_to_cmd_field() {
        assert_eq!(
            extract_command_text(&serde_json::json!({"cmd": "ls"})),
            Some("ls".to_string())
        );
    }

    #[test]
    fn extract_command_text_returns_none_when_absent() {
        assert_eq!(
            extract_command_text(&serde_json::json!({"path": "x"})),
            None
        );
        assert_eq!(extract_command_text(&serde_json::json!({})), None);
    }

    // === command_summary / network_summary ===

    #[test]
    fn command_summary_with_command_or_cmd() {
        assert_eq!(
            command_summary(&serde_json::json!({"command": "ls"})),
            "执行命令 ls"
        );
        assert_eq!(
            command_summary(&serde_json::json!({"cmd": "pwd"})),
            "执行命令 pwd"
        );
    }

    #[test]
    fn command_summary_without_command_field() {
        assert_eq!(command_summary(&serde_json::json!({})), "执行命令");
    }

    #[test]
    fn network_summary_with_or_without_url() {
        assert_eq!(
            network_summary(&serde_json::json!({"url": "https://example.com"})),
            "访问网络 https://example.com"
        );
        assert_eq!(network_summary(&serde_json::json!({})), "访问网络");
    }

    // === matches_pre_approved 补充 ===

    #[test]
    fn matches_pre_approved_empty_list_returns_false() {
        assert!(!matches_pre_approved(
            "shell.run",
            &serde_json::json!({"command": "x"}),
            &[],
        ));
    }

    #[test]
    fn matches_pre_approved_cmd_field_also_works() {
        let allowed = vec![PreApprovedPrompt {
            tool: "shell.run".to_string(),
            prompt: "ls".to_string(),
        }];
        assert!(matches_pre_approved(
            "shell.run",
            &serde_json::json!({"cmd": "ls -la"}),
            &allowed,
        ));
    }

    #[test]
    fn matches_pre_approved_no_command_text_returns_false() {
        let allowed = vec![PreApprovedPrompt {
            tool: "fs.write".to_string(),
            prompt: "x".to_string(),
        }];
        // fs.write 无 command/cmd 字段 → command_text 为 None → 不匹配
        assert!(!matches_pre_approved(
            "fs.write",
            &serde_json::json!({"path": "x"}),
            &allowed,
        ));
    }

    // === is_instructional_content 全分支正向测试 ===

    #[test]
    fn is_instructional_content_english_positive_all_branches() {
        assert!(is_instructional_content("Always use cargo fmt"));
        assert!(is_instructional_content("never commit secrets"));
        assert!(is_instructional_content("must run tests"));
        assert!(is_instructional_content("do not push to main"));
        assert!(is_instructional_content("don't use unwrap"));
        assert!(is_instructional_content("should be tested"));
        assert!(is_instructional_content("## Rules"));
        assert!(is_instructional_content("## Constraints"));
    }

    #[test]
    fn is_instructional_content_chinese_positive_all_branches() {
        assert!(is_instructional_content("总是使用 cargo fmt"));
        assert!(is_instructional_content("永远不要提交密钥"));
        assert!(is_instructional_content("禁止使用 unwrap"));
        assert!(is_instructional_content("必须运行测试"));
        assert!(is_instructional_content("不要直接修改"));
        assert!(is_instructional_content("不得绕过权限"));
        assert!(is_instructional_content("应当遵循规范"));
        assert!(is_instructional_content("应保持简洁"));
        assert!(is_instructional_content("## 规则"));
        assert!(is_instructional_content("## 约束"));
    }

    #[test]
    fn is_instructional_content_case_insensitive_english() {
        assert!(is_instructional_content("NEVER commit secrets"));
        assert!(is_instructional_content("MUST RUN TESTS"));
        assert!(is_instructional_content("## RULES"));
    }

    #[test]
    fn is_instructional_content_multiline_with_one_matching_line() {
        let content = "project uses rust 2024\n\nmust run tests before commit\n";
        assert!(is_instructional_content(content));
    }

    #[test]
    fn is_instructional_content_only_empty_lines_returns_false() {
        assert!(!is_instructional_content("\n\n\n"));
    }

    // === full_options / project_doc_options ===

    #[test]
    fn full_options_contains_all_four_options() {
        let opts = full_options();
        assert_eq!(opts.len(), 4);
        assert!(opts.contains(&PromptOption::AllowOnce));
        assert!(opts.contains(&PromptOption::AllowAlways));
        assert!(opts.contains(&PromptOption::DenyOnce));
        assert!(opts.contains(&PromptOption::DenyAlways));
    }

    #[test]
    fn project_doc_options_excludes_allow_always() {
        let opts = project_doc_options();
        assert_eq!(opts.len(), 3);
        assert!(opts.contains(&PromptOption::AllowOnce));
        assert!(!opts.contains(&PromptOption::AllowAlways));
        assert!(opts.contains(&PromptOption::DenyOnce));
        assert!(opts.contains(&PromptOption::DenyAlways));
    }

    // === next_prompt_id / make_prompt ===

    #[test]
    fn next_prompt_id_is_unique_and_prefixed() {
        let id1 = next_prompt_id();
        let id2 = next_prompt_id();
        assert!(id1.starts_with("prompt-"));
        assert_ne!(id1, id2);
    }

    #[test]
    fn make_prompt_populates_all_fields() {
        let prompt = make_prompt(
            "fs.write",
            "写入文件".to_string(),
            Risk::Medium,
            full_options(),
        );
        assert_eq!(prompt.tool, "fs.write");
        assert_eq!(prompt.summary, "写入文件");
        assert_eq!(prompt.risk, Risk::Medium);
        assert_eq!(prompt.options.len(), 4);
        assert!(prompt.id.starts_with("prompt-"));
    }

    // === compute_verdict 直接测试（覆盖各分支）===

    #[test]
    fn compute_verdict_none_side_effect_returns_allow() {
        let ctx = ctx_with_mode(SideEffect::None, PermissionMode::Default);
        let verdict = compute_verdict("fs.read", &serde_json::json!({"path": "x"}), &ctx);
        assert!(matches!(verdict, Verdict::Allow));
    }

    #[test]
    fn compute_verdict_command_returns_ask() {
        let ctx = ctx_with_mode(SideEffect::Command, PermissionMode::Default);
        let verdict = compute_verdict("shell.run", &serde_json::json!({"command": "ls"}), &ctx);
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[test]
    fn compute_verdict_network_returns_ask() {
        let ctx = ctx_with_mode(SideEffect::Network, PermissionMode::Default);
        let verdict = compute_verdict("web.fetch", &serde_json::json!({"url": "https://x"}), &ctx);
        assert!(matches!(verdict, Verdict::Ask(_)));
    }

    #[test]
    fn compute_verdict_blacklist_highest_priority() {
        let ctx = ctx_file_write();
        let verdict = compute_verdict("fs.delete", &serde_json::json!({"path": "AGENTS.md"}), &ctx);
        match verdict {
            Verdict::Deny(msg) => assert!(msg.contains("blacklisted")),
            other => panic!("期望黑名单 Deny，实际 {other:?}"),
        }
    }

    #[test]
    fn compute_verdict_pre_approved_overrides_ask() {
        let ctx = ctx_with_allowed(
            SideEffect::Command,
            vec![PreApprovedPrompt {
                tool: "shell.run".to_string(),
                prompt: "cargo build".to_string(),
            }],
        );
        let verdict = compute_verdict(
            "shell.run",
            &serde_json::json!({"command": "cargo build --release"}),
            &ctx,
        );
        assert!(matches!(verdict, Verdict::Allow));
    }
}
