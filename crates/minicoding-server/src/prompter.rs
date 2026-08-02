//! `ServerPrompter`：HTTP 端权限交互器（T-M8-2）。
//!
//! 实现 `PermissionPrompter` trait，把权限询问转为异步等待——询问时：
//! 1. 生成唯一 `permission_id`（若 prompt 未带 id）；
//! 2. 在 `SessionManager` 的 pending map 中注册 `oneshot::Sender<Decision>`；
//! 3. `Runtime` 通过 `Event::PermissionRequested` 广播询问到所有 SSE 订阅者；
//! 4. `ServerPrompter::prompt` 的 future 在 `oneshot::Receiver` 上挂起；
//! 5. 客户端通过 `POST /sessions/{id}/permissions/{pid}` 回传决策，
//!    `SessionManager::resolve_permission` 查找 pending map 并发送决策；
//! 6. `prompt` future 收到决策后返回，工具调用继续（或被拒绝）。
//!
//! 超时保护：默认 300s 无响应则返回 `Deny`（避免永久挂起阻塞 Runtime）。
//! 超时时长由 `ServerConfig::permission_timeout_sec` 控制。

use minicoding_core::policy::{Decision, PermissionPrompt, PermissionPrompter};
use minicoding_core::provider::BoxFuture;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;

/// pending 权限请求表（`permission_id` → `oneshot::Sender<Decision>`）。
///
/// 每个 `ServerSession` 持有一个 `PendingPermissions`。`ServerPrompter::prompt`
/// 注册 sender，`SessionManager::resolve_permission` 发送 decision 后移除条目。
pub type PendingPermissions = Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>;

/// HTTP 端权限交互器。
///
/// 每个 `ServerSession` 构造一个 `ServerPrompter`，共享该 session 的
/// `PendingPermissions`。`prompt` 调用时注册 oneshot sender，await reply
/// 或超时。
///
/// 与 `InteractivePrompter`（读 stdin）/ `CallbackPrompter`（同步闭包）的区别：
/// `ServerPrompter` 是异步的——决策来自外部 HTTP 请求，不可同步获取。
#[derive(Clone)]
pub struct ServerPrompter {
    pending: PendingPermissions,
    timeout_dur: Duration,
}

impl ServerPrompter {
    /// 创建 `ServerPrompter`。
    ///
    /// `pending` 与 `SessionManager` 共享（`SessionManager::resolve_permission`
    /// 通过此 map 发送决策）。
    #[must_use]
    pub fn new(pending: PendingPermissions, timeout_dur: Duration) -> Self {
        Self {
            pending,
            timeout_dur,
        }
    }
}

impl std::fmt::Debug for ServerPrompter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerPrompter")
            .field("timeout", &self.timeout_dur)
            .finish_non_exhaustive()
    }
}

impl PermissionPrompter for ServerPrompter {
    fn prompt(&self, req: PermissionPrompt) -> BoxFuture<'_, Decision> {
        let pending = self.pending.clone();
        let timeout_dur = self.timeout_dur;
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            let perm_id = req.id.clone();
            {
                let mut guard = pending.lock().await;
                guard.insert(perm_id.clone(), tx);
            }
            // 等待客户端通过 HTTP 回传决策，或超时返回 Deny。
            // 超时保护：避免客户端断连后 turn 永久挂起。
            match timeout(timeout_dur, rx).await {
                Ok(Ok(decision)) => decision,
                Ok(Err(_)) => {
                    // sender drop（理论不可达：sender 在 mutex guard 中，只有
                    // resolve_permission 或超时移除时 drop）
                    tracing::warn!(
                        permission_id = %perm_id,
                        "permission sender dropped unexpectedly"
                    );
                    Decision::Deny("permission channel closed".to_string())
                }
                Err(_) => {
                    // 超时：移除 pending 条目，返回 Deny
                    let mut guard = pending.lock().await;
                    guard.remove(&perm_id);
                    tracing::warn!(
                        permission_id = %perm_id,
                        timeout_sec = timeout_dur.as_secs(),
                        "permission request timed out, denying"
                    );
                    Decision::Deny("permission request timed out".to_string())
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

    #[tokio::test]
    async fn resolve_permission_returns_decision() {
        let pending: PendingPermissions = Arc::new(Mutex::new(HashMap::new()));
        let prompter = ServerPrompter::new(pending.clone(), Duration::from_secs(5));

        // prompt() 返回 BoxFuture，需被 poll 才会注册 sender 到 pending map。
        // 用 select! 确保第一次 poll 执行 insert，然后分支到 sleep 让控制权返回。
        let prompt_fut = prompter.prompt(sample_prompt("perm-1"));
        tokio::pin!(prompt_fut);
        tokio::select! {
            _ = &mut prompt_fut => panic!("prompt should not complete before resolution"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }

        // 从 pending map 取出 sender，发送决策
        let tx = {
            let mut guard = pending.lock().await;
            guard.remove("perm-1")
        };
        assert!(tx.is_some(), "pending permission should be registered");
        tx.unwrap().send(Decision::Allow).expect("send decision");

        let decision = prompt_fut.await;
        assert_eq!(decision, Decision::Allow);
    }

    #[tokio::test]
    async fn timeout_returns_deny() {
        let pending: PendingPermissions = Arc::new(Mutex::new(HashMap::new()));
        let prompter = ServerPrompter::new(pending, Duration::from_millis(50));

        let decision = prompter.prompt(sample_prompt("perm-timeout")).await;
        match decision {
            Decision::Deny(msg) => assert!(msg.contains("timed out")),
            _ => panic!("expected Deny on timeout"),
        }
    }
}
