//! 权限交互器实现（`PermissionPrompter`）。
//!
//! - [`NonInteractivePrompter`]：始终 `Deny`，CI/脚本的安全默认；
//! - [`InteractivePrompter`]：stderr 打印风险摘要后从 stdin 读 `y/n` 确认。
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
            eprintln!("[permission] {} (risk: {:?})", req.summary, req.risk);
            eprintln!("[permission] allow? [y/N] ");
            match tokio::task::spawn_blocking(read_yes_no).await {
                Ok(true) => Decision::Allow,
                Ok(false) => Decision::Deny("denied by user".to_string()),
                Err(_) => Decision::Deny("prompter task failed".to_string()),
            }
        })
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
