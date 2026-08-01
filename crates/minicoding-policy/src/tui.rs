//! TUI 交互器（T-M7-3）。
//!
//! [`TuiPrompter`] 实现 [`PermissionPrompter`]，通过 mpsc channel 把权限询问
//! 发往 TUI 主循环（点对点，非 broadcast），UI 渲染弹窗后通过 oneshot 回传
//! [`Decision`]。`prompt` 返回的 future 在 await oneshot receiver 时挂起，
//! 工具调用阻塞但 Runtime 调度器仍可推进其他 task（如 `EventBus` 转发）。
//!
//! ## 线程模型
//!
//! `TuiPrompter` 在 Runtime 桥接线程（`current_thread` + `LocalSet`）上被
//! `Runtime::run_turn` 调用；`tx.send(...).await` 不阻塞调度器，receiver 由
//! TUI 主循环消费（经 `runtime_bridge` 转发为 `AppEvent::PermissionRequest`）。

use minicoding_core::policy::{
    Decision, PermissionPrompt, PermissionPrompter, TuiPermissionRequest,
};
use minicoding_core::provider::BoxFuture;
use tokio::sync::{mpsc, oneshot};

/// TUI 权限交互器（T-M7-3）。
///
/// 持有发往 TUI 主循环的 mpsc sender。`prompt` 创建 oneshot channel，把
/// [`TuiPermissionRequest`] 发给 UI，然后 await receiver——UI 渲染弹窗、用户
/// 按键后通过 `reply` 回传 [`Decision`]，future 解挂返回。
///
/// channel 关闭（UI 退出）时返回 `Decision::Deny`，避免工具调用永久挂起。
///
/// # 示例
///
/// ```no_run
/// use minicoding_core::policy::PermissionPrompter;
/// use minicoding_policy::TuiPrompter;
/// use tokio::sync::mpsc;
///
/// # async fn docs() {
/// let (tx, mut rx) = mpsc::channel(8);
/// let prompter = TuiPrompter::new(tx);
/// // UI 端：消费 rx 上的 TuiPermissionRequest，渲染弹窗，回传 Decision
/// # }
/// ```
#[derive(Clone)]
pub struct TuiPrompter {
    tx: mpsc::Sender<TuiPermissionRequest>,
}

impl TuiPrompter {
    /// 创建 TUI 交互器，传入发往 UI 的 channel sender。
    ///
    /// Channel 容量建议 8：权限询问不会高频突发，且 UI 一次只处理一个弹窗。
    #[must_use]
    pub fn new(tx: mpsc::Sender<TuiPermissionRequest>) -> Self {
        Self { tx }
    }
}

impl PermissionPrompter for TuiPrompter {
    fn prompt(&self, prompt: PermissionPrompt) -> BoxFuture<'_, Decision> {
        let tx = self.tx.clone();
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            let req = TuiPermissionRequest {
                prompt,
                reply: reply_tx,
            };
            // channel 关闭（UI 退出）或 send 失败 → 拒绝，避免工具调用永久挂起
            if tx.send(req).await.is_err() {
                return Decision::Deny("TUI 已关闭".to_string());
            }
            match reply_rx.await {
                Ok(d) => d,
                Err(_) => Decision::Deny("TUI 未响应".to_string()),
            }
        })
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
    async fn tui_prompter_allow_after_reply() {
        let (tx, mut rx) = mpsc::channel(8);
        let prompter = TuiPrompter::new(tx);
        // 先启动 prompt，再在另一 task 回复 Allow
        let prompter_clone = prompter.clone();
        let handle =
            tokio::spawn(async move { prompter_clone.prompt(sample_prompt("fs.write")).await });
        let req = rx.recv().await.expect("应收到询问");
        req.reply.send(Decision::Allow).expect("回传决策");
        let decision = handle.await.expect("task panicked");
        assert_eq!(decision, Decision::Allow);
    }

    #[tokio::test]
    async fn tui_prompter_deny_on_closed_channel() {
        let (tx, rx) = mpsc::channel(8);
        let prompter = TuiPrompter::new(tx);
        drop(rx); // 模拟 UI 关闭
        let decision = prompter.prompt(sample_prompt("fs.write")).await;
        assert_eq!(decision, Decision::Deny("TUI 已关闭".to_string()));
    }

    #[tokio::test]
    async fn tui_prompter_deny_on_dropped_reply() {
        let (tx, mut rx) = mpsc::channel(8);
        let prompter = TuiPrompter::new(tx);
        let prompter_clone = prompter.clone();
        let handle =
            tokio::spawn(async move { prompter_clone.prompt(sample_prompt("shell.run")).await });
        let req = rx.recv().await.expect("应收到询问");
        // 不回复，直接 drop reply sender（模拟 UI 异常退出）
        drop(req.reply);
        let decision = handle.await.expect("task panicked");
        assert_eq!(decision, Decision::Deny("TUI 未响应".to_string()));
    }
}
