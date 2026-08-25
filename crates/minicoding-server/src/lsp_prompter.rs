//! `LspPrompter`：LSP 端权限交互器（T-M8-9）。
//!
//! 实现 `PermissionPrompter` trait，通过 LSP `window/showMessageRequest` 完成
//! 点对点权限交互。与 `TuiPrompter`/`InteractivePrompter`/`ServerPrompter` 同构，
//! 只是交互通道不同：
//!
//! - `InteractivePrompter`：读 stdin（CLI TTY）
//! - `TuiPrompter`：mpsc channel 到 TUI 主循环
//! - `ServerPrompter`：oneshot + HTTP POST 回传（HTTP/SSE server）
//! - `LspPrompter`：mpsc channel 到 LSP server 主循环，后者调 `window/showMessageRequest`
//!
//! ## 设计要点
//!
//! - **通道解耦**：`LspPrompter` 不持有 `tower_lsp::Client`（Client 在 `LspService::new`
//!   闭包中才可用，而 `LspPrompter` 需在 `Runtime` 构造前注入）。用 `mpsc::Sender`
//!   把权限请求转发到 LSP server 的 prompter loop，server 端持有 Client 调
//!   `showMessageRequest`，结果通过 `oneshot` 回传。
//! - **动作映射**：`Allow`/`Deny`/`AllowAlways` → `Decision::Allow`（Decision enum
//!   只有 Allow/Deny，"always" 语义由 `PermissionPolicy` 缓存层处理）。
//! - **超时保护**：与 `ServerPrompter` 一致，默认 300s 无响应返回 `Deny`。
//! - **审计**：权限决策落 `audit.log`（由 `Runtime` 在 `execute_side_effect_call`
//!   中统一记录，`LspPrompter` 只返回决策）。
//!
//! 详见 `docs/dev-plan.md` T-M8-9、`docs/design.md` §24。

use minicoding_core::policy::{Decision, PermissionPrompt, PermissionPrompter};
use minicoding_core::provider::BoxFuture;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// 从 `LspPrompter` 发往 LSP server prompter loop 的权限请求。
pub(crate) struct PermissionRequest {
    /// 权限询问详情（工具名/摘要/风险/选项）。
    pub prompt: PermissionPrompt,
    /// 回传决策的 oneshot 通道。
    pub reply: oneshot::Sender<Decision>,
}

/// LSP 端权限交互器。
///
/// 持有 `mpsc::Sender`，每次 `prompt` 调用：
/// 1. 创建 `oneshot` 通道；
/// 2. 通过 `mpsc` 发送 `PermissionRequest` 到 LSP server 的 prompter loop；
/// 3. await reply 或超时。
///
/// LSP server 的 prompter loop 收到请求后，调 `Client::show_message_request`，
/// 根据用户选择发送 `Decision` 回 `oneshot`。
#[derive(Clone)]
pub struct LspPrompter {
    tx: mpsc::Sender<PermissionRequest>,
    timeout_dur: Duration,
}

impl LspPrompter {
    /// 创建 `LspPrompter`。
    ///
    /// `tx` 与 LSP server 的 prompter loop 共享（loop 端持有 `mpsc::Receiver`）。
    #[must_use]
    pub(crate) fn new(tx: mpsc::Sender<PermissionRequest>, timeout_dur: Duration) -> Self {
        Self { tx, timeout_dur }
    }
}

impl std::fmt::Debug for LspPrompter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspPrompter")
            .field("timeout", &self.timeout_dur)
            .finish_non_exhaustive()
    }
}

impl PermissionPrompter for LspPrompter {
    fn prompt(&self, req: PermissionPrompt) -> BoxFuture<'_, Decision> {
        let tx = self.tx.clone();
        let timeout_dur = self.timeout_dur;
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            let perm_id = req.id.clone();
            let msg = PermissionRequest {
                prompt: req,
                reply: reply_tx,
            };
            // 发送请求到 LSP server prompter loop
            if tx.send(msg).await.is_err() {
                tracing::warn!(
                    permission_id = %perm_id,
                    "LSP prompter channel closed, denying"
                );
                return Decision::Deny("LSP prompter channel closed".to_string());
            }
            // 等待 LSP server 回传决策，或超时返回 Deny
            match timeout(timeout_dur, reply_rx).await {
                Ok(Ok(decision)) => decision,
                Ok(Err(_)) => {
                    tracing::warn!(
                        permission_id = %perm_id,
                        "LSP prompter reply channel closed unexpectedly"
                    );
                    Decision::Deny("LSP prompter reply channel closed".to_string())
                }
                Err(_) => {
                    tracing::warn!(
                        permission_id = %perm_id,
                        timeout_sec = timeout_dur.as_secs(),
                        "LSP permission request timed out, denying"
                    );
                    Decision::Deny("LSP permission request timed out".to_string())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::policy::{PromptOption, Risk};

    fn sample_prompt(id: &str) -> PermissionPrompt {
        PermissionPrompt {
            id: id.to_string(),
            tool: "fs.write".to_string(),
            summary: "write to /tmp/test.txt".to_string(),
            risk: Risk::Medium,
            options: vec![PromptOption::AllowOnce, PromptOption::DenyOnce],
        }
    }

    // F1：start_paused——select! 的 10ms 等待仅为了让 prompt_fut 完成首次
    // poll（请求入 channel），虚拟时钟下即时推进，语义不变。
    #[tokio::test(start_paused = true)]
    async fn prompt_returns_decision_from_server_loop() {
        let (tx, mut rx) = mpsc::channel::<PermissionRequest>(8);
        let prompter = LspPrompter::new(tx, Duration::from_secs(5));

        // 启动 prompt future
        let prompt_fut = prompter.prompt(sample_prompt("perm-1"));
        tokio::pin!(prompt_fut);

        // 让 prompt_fut 跑一会儿，发送请求到 channel
        tokio::select! {
            _ = &mut prompt_fut => panic!("prompt should not complete before reply"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }

        // 从 channel 取出请求，发送决策
        let req = rx.recv().await.expect("should receive permission request");
        assert_eq!(req.prompt.id, "perm-1");
        req.reply.send(Decision::Allow).expect("send decision");

        // prompt future 应返回 Allow
        let decision = prompt_fut.await;
        assert_eq!(decision, Decision::Allow);
    }

    #[tokio::test]
    async fn prompt_returns_deny_when_channel_closed() {
        let (tx, rx) = mpsc::channel::<PermissionRequest>(8);
        drop(rx); // 关闭接收端
        let prompter = LspPrompter::new(tx, Duration::from_secs(5));

        let decision = prompter.prompt(sample_prompt("perm-2")).await;
        match decision {
            Decision::Deny(msg) => assert!(msg.contains("channel closed")),
            _ => panic!("expected Deny when channel closed"),
        }
    }

    #[tokio::test]
    async fn prompt_returns_deny_on_timeout() {
        let (tx, _rx) = mpsc::channel::<PermissionRequest>(8);
        // 50ms 超时——不回复，应超时返回 Deny
        let prompter = LspPrompter::new(tx, Duration::from_millis(50));

        let decision = prompter.prompt(sample_prompt("perm-timeout")).await;
        match decision {
            Decision::Deny(msg) => assert!(msg.contains("timed out")),
            _ => panic!("expected Deny on timeout"),
        }
    }
}
