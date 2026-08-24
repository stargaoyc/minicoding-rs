//! 权限交互器实现（`PermissionPrompter`）。
//!
//! - [`NonInteractivePrompter`]：始终 `Deny`，CI/脚本的安全默认；
//! - [`AutoApprovePrompter`]：始终 `Allow`，`exec` 批量执行模式（显式意图 + 审计记录）；
//! - [`InteractivePrompter`]：stderr 打印风险摘要后从 stdin 读 `y/n` 确认；
//! - [`CallbackPrompter`]：闭包注入，供 M8 SDK 嵌入使用（T-M4-11）。
//!
//! 决策（`PermissionPolicy`）与交互（`Prompter`）分离，见 `docs/design.md` §9.1。

use minicoding_core::policy::{Decision, PermissionPrompt, PermissionPrompter};
use minicoding_core::provider::BoxFuture;

/// 非交互式交互器：始终拒绝。
///
/// 适用于 CI、脚本、无人值守场景，作为安全默认——任何副作用询问都不会被自动放行。
pub struct NonInteractivePrompter;

impl NonInteractivePrompter {
    /// 创建非交互式交互器。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for NonInteractivePrompter {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionPrompter for NonInteractivePrompter {
    fn prompt(&self, _req: PermissionPrompt) -> BoxFuture<'_, Decision> {
        Box::pin(async move { Decision::Deny("non-interactive mode".to_string()) })
    }
}

/// 批量执行交互器：始终允许。
///
/// 供 `minicoding exec` 使用——用户显式声明"非交互批量执行"，策略层
/// （`BuiltinPolicy` + 沙箱策略）仍校验越界/黑名单（C-03），每次决策仍由
/// Runtime 落 `audit.log`（C-01 在实现层不被绕过）。
pub struct AutoApprovePrompter;

impl AutoApprovePrompter {
    /// 创建批量执行交互器。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for AutoApprovePrompter {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionPrompter for AutoApprovePrompter {
    fn prompt(&self, _req: PermissionPrompt) -> BoxFuture<'_, Decision> {
        Box::pin(async move { Decision::Allow })
    }
}

/// 交互式交互器：stderr 提示后从 stdin 读取 `y/n` 确认。
///
/// stdin 为阻塞 IO，通过 `tokio::task::spawn_blocking` 包裹读取，避免阻塞
/// 异步运行时线程（AGENTS.md §2.4：阻塞调用包裹线程）。
pub struct InteractivePrompter;

impl InteractivePrompter {
    /// 创建交互式交互器。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for InteractivePrompter {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionPrompter for InteractivePrompter {
    fn prompt(&self, req: PermissionPrompt) -> BoxFuture<'_, Decision> {
        Box::pin(async move {
            // 遗留#3：prompt 提供 Always 选项时开放 `a` 键（始终允许/拒绝）
            let has_always = req.options.iter().any(|o| {
                matches!(
                    o,
                    minicoding_core::policy::PromptOption::AllowAlways
                        | minicoding_core::policy::PromptOption::DenyAlways
                )
            });
            let deny_always_offered = req
                .options
                .contains(&minicoding_core::policy::PromptOption::DenyAlways);
            if has_always {
                eprintln!("[permission] {} (risk: {:?})", req.summary, req.risk);
                if deny_always_offered {
                    eprintln!("[permission] [y]允许 / [a]始终允许 / [n]拒绝 / [N]始终拒绝");
                } else {
                    eprintln!("[permission] [y]允许 / [a]始终允许 / [n/N]拒绝");
                }
                match tokio::task::spawn_blocking(read_ynad).await {
                    Ok(Some(Decision::Allow)) => Decision::Allow,
                    Ok(Some(d @ (Decision::AllowAlways | Decision::DenyAlways(_)))) => d,
                    _ => Decision::Deny("denied by user".to_string()),
                }
            } else {
                eprintln!("[permission] {} (risk: {:?})", req.summary, req.risk);
                eprintln!("[permission] allow? [y/N] ");
                match tokio::task::spawn_blocking(read_yes_no).await {
                    Ok(true) => Decision::Allow,
                    Ok(false) => Decision::Deny("denied by user".to_string()),
                    Err(_) => Decision::Deny("prompter task failed".to_string()),
                }
            }
        })
    }
}

/// 读取一行并解析 y/a/n（遗留#3）：`Some(Allow)`=y、`Some(AllowAlways)`=a，
/// 其余（n/N/空/无法解析）=一次性 [`Decision::Deny`]。`DenyAlways` 不经键盘
/// 映射——CLI 提示层仅在选项集含 `DenyAlways` 时提示 `N`，当前 v1 统一折叠
/// 为一次性 Deny（持久化拒绝规则由 Web/TUI 的显式按钮路径写入）。
fn read_ynad() -> Option<Decision> {
    use std::io::BufRead;
    let mut line = String::new();
    let stdin = std::io::stdin();
    if stdin.lock().read_line(&mut line).is_err() {
        return None;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(Decision::Allow),
        "a" => Some(Decision::AllowAlways),
        _ => Some(Decision::Deny("denied by user".to_string())),
    }
}

/// 从 stdin 读取一行并判定是否为肯定回答（`y`/`yes`，大小写不敏感）。
fn read_yes_no() -> bool {
    use std::io::BufRead;
    let mut line = String::new();
    let stdin = std::io::stdin();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    let trimmed = line.trim();
    trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes")
}

/// 闭包注入式交互器（T-M4-11，供 M8 SDK 嵌入使用）。
///
/// SDK 调用方提供同步闭包 `Fn(PermissionPrompt) -> Decision`，由 `CallbackPrompter`
/// 在 `prompt` 调用时同步执行并通过 `BoxFuture` 包装返回。闭包捕获的上下文（如
/// GUI 事件循环、RPC 回调句柄）由调用方负责 `Send + Sync`。
///
/// 与 `InteractivePrompter` 的区别：`InteractivePrompter` 直接读 stdin，仅适用
/// CLI；`CallbackPrompter` 把交互方式交给嵌入方，便于 SDK 适配任意 UI/RPC 后端。
///
/// # 示例
///
/// ```
/// use minicoding_core::policy::{Decision, PermissionPrompter, PermissionPrompt, Risk};
/// use minicoding_policy::CallbackPrompter;
///
/// let prompter = CallbackPrompter::new(|_req| Decision::Allow);
/// # // 静态闭包满足 Send + Sync 要求
/// ```
pub struct CallbackPrompter<F>
where
    F: Fn(PermissionPrompt) -> Decision + Send + Sync,
{
    callback: F,
}

impl<F> CallbackPrompter<F>
where
    F: Fn(PermissionPrompt) -> Decision + Send + Sync,
{
    /// 创建闭包交互器。
    #[must_use]
    pub fn new(callback: F) -> Self {
        Self { callback }
    }
}

impl<F> PermissionPrompter for CallbackPrompter<F>
where
    F: Fn(PermissionPrompt) -> Decision + Send + Sync,
{
    fn prompt(&self, req: PermissionPrompt) -> BoxFuture<'_, Decision> {
        let decision = (self.callback)(req);
        Box::pin(async move { decision })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::policy::{PromptOption, Risk};

    fn sample_prompt(tool: &str) -> PermissionPrompt {
        PermissionPrompt {
            id: "test".to_string(),
            tool: tool.to_string(),
            summary: "test summary".to_string(),
            risk: Risk::Medium,
            options: vec![PromptOption::AllowOnce, PromptOption::DenyOnce],
        }
    }

    #[tokio::test]
    async fn callback_prompter_allow() {
        let prompter = CallbackPrompter::new(|_req| Decision::Allow);
        let decision = prompter.prompt(sample_prompt("fs.write")).await;
        assert_eq!(decision, Decision::Allow);
    }

    #[tokio::test]
    async fn callback_prompter_deny() {
        let prompter = CallbackPrompter::new(|req| Decision::Deny(format!("denied: {}", req.tool)));
        let decision = prompter.prompt(sample_prompt("shell.run")).await;
        assert_eq!(decision, Decision::Deny("denied: shell.run".to_string()));
    }

    #[tokio::test]
    async fn callback_prompter_inspects_request() {
        // 闭包可读取请求字段做风险感知决策
        let prompter = CallbackPrompter::new(|req| {
            if req.risk == Risk::High {
                Decision::Deny("high risk".to_string())
            } else {
                Decision::Allow
            }
        });
        let low = prompter
            .prompt(PermissionPrompt {
                id: "1".into(),
                tool: "fs.read".into(),
                summary: "low".into(),
                risk: Risk::Low,
                options: vec![PromptOption::AllowOnce],
            })
            .await;
        assert_eq!(low, Decision::Allow);

        let high = prompter
            .prompt(PermissionPrompt {
                id: "2".into(),
                tool: "shell.run".into(),
                summary: "high".into(),
                risk: Risk::High,
                options: vec![PromptOption::AllowOnce],
            })
            .await;
        assert_eq!(high, Decision::Deny("high risk".to_string()));
    }
}
