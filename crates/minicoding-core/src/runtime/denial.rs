//! 沙箱拒绝检测与初始化失败回退（A-2026-08 自 rt.rs 抽出；M-05/M-09，
//! 见 `security.md` §8、`rules.md` C-30）。
//!
//! 两类独立的沙箱异常：
//! - **拒绝**（denial）：内核级硬反馈（EPERM/EACCES/landlock），行为被沙箱正确
//!   拦截——更新熔断器（C-30 不可被 LLM 绕过）并回灌结构化错误；
//! - **初始化失败**（setup failure）：沙箱机制本身故障（如 Windows Job Object
//!   恢复线程竞态）——经用户显式确认后允许沙箱外重试一次（C-22 red 警告语义）。

use super::Event;
use super::rt::Runtime;
use crate::metrics;
use crate::model::{ToolCall, ToolCallId, ToolResult};
use crate::policy::{Decision, PermissionPrompt};
use crate::sandbox::{BreakerState, SandboxPolicy};
use crate::tool::ToolContext;

impl Runtime {
    /// 判断工具错误是否为"沙箱初始化失败"（`apply`/`post_spawn`）。
    ///
    /// 与沙箱拒绝（EPERM/EACCES，`handle_sandbox_denial`）区分：初始化失败是沙箱
    /// 机制本身故障（如 Windows Job Object 恢复线程快照竞态），不是被沙箱拦下的
    /// 行为，可通过沙箱外重试规避。
    fn is_sandbox_setup_failure(error: &crate::model::ToolError) -> bool {
        match error {
            crate::model::ToolError::Exec(msg) => {
                msg.starts_with("sandbox apply failed")
                    || msg.starts_with("sandbox post_spawn failed")
            }
            _ => false,
        }
    }

    /// 沙箱初始化失败时询问用户是否在沙箱外重试（C-22：用户显式选定 + High risk 警告）。
    ///
    /// 允许 → 返回以 `DangerFullAccess` 策略构造的重试上下文（同一 driver，该策略下
    /// `apply`/`post_spawn` 均为 no-op）；拒绝或非沙箱初始化错误 → `None`（调用方按原
    /// 错误处理）。询问与决策经 `PermissionRequested`/`PermissionResolved` 事件广播
    /// （前端弹窗复用 W-03 权限链路）并落 `audit.log`（AGENTS.md §5.5）。
    pub(crate) async fn maybe_sandbox_fallback(
        &self,
        call: &ToolCall,
        error: &crate::model::ToolError,
        ctx: &ToolContext,
    ) -> Option<ToolContext> {
        if !Self::is_sandbox_setup_failure(error) {
            return None;
        }
        tracing::warn!(
            tool = %call.name,
            call_id = %call.id,
            error = %error,
            "sandbox setup failed, prompting user for out-of-sandbox retry"
        );
        let prompt = PermissionPrompt {
            id: format!("sbx-{}", uuid::Uuid::new_v4()),
            tool: call.name.clone(),
            summary: format!(
                "OS 沙箱初始化失败（{error}）。\n是否在沙箱外运行此命令？\n\
                 ⚠ 沙箱外运行 = 放弃 OS 级隔离（C-22），仅限受信环境！"
            ),
            risk: crate::policy::Risk::High,
            options: vec![
                crate::policy::PromptOption::AllowOnce,
                crate::policy::PromptOption::DenyOnce,
            ],
        };
        let prompt_id = prompt.id.clone();
        self.events.emit(Event::PermissionRequested {
            id: prompt.id.clone(),
            tool: prompt.tool.clone(),
            summary: prompt.summary.clone(),
            risk: prompt.risk,
        });
        let decision = self.prompter.prompt(prompt.clone()).await;
        let event = Event::PermissionResolved {
            id: prompt_id.clone(),
            decision: decision.clone(),
        };
        self.persist_event(&event).await;
        self.events.emit(event);
        // 审计：沙箱外回退决策必须落盘（与普通权限决策同等对待，AGENTS.md §5.5）
        self.record_permission_audit(
            &format!("{} sandbox-fallback", call.name),
            &decision,
            Some(prompt_id),
            None,
        )
        .await;
        match decision {
            Decision::Allow | Decision::AllowAlways => {
                let mut fallback = ctx.clone();
                fallback.sandbox_policy = Some(SandboxPolicy::DangerFullAccess);
                Some(fallback)
            }
            Decision::Deny(_) | Decision::DenyAlways(_) => None,
        }
    }

    /// 沙箱拒绝检测与熔断处理（T-M4-5）。
    ///
    /// 检测工具错误是否为沙箱拒绝（EPERM/EACCES/landlock 等）。若是：
    /// - 更新熔断器计数；
    /// - 软熔断（≥3 次）：附加方向提醒返回；
    /// - 硬熔断（≥5 次）：返回带总结的错误；
    /// - 未熔断：返回带 denial 标识的错误，提示 LLM/用户。
    ///
    /// 返回 `Some(ToolResult)` 表示已识别为 denial 并生成回灌结果；
    /// 返回 `None` 表示非 denial，调用方原样传播错误。
    pub(crate) fn handle_sandbox_denial(
        &self,
        call_id: &ToolCallId,
        tool: &str,
        error: &crate::model::ToolError,
    ) -> Option<(ToolCallId, ToolResult)> {
        Self::build_denial_result(
            self.denial_detector.as_ref(),
            self.sandbox_breaker.as_ref(),
            tool,
            error,
        )
        .map(|r| (call_id.clone(), r))
    }

    /// 沙箱拒绝检测（M-09 起为静态辅助：只读并行桶与副作用串行路径共用）。
    ///
    /// 检测工具错误是否为沙箱拒绝（EPERM/EACCES/landlock 等）。若是：
    /// - 更新熔断器计数；
    /// - 软熔断（≥3 次）：附加方向提醒返回；
    /// - 硬熔断（≥5 次）：返回带总结的错误；
    /// - 未熔断：返回带 denial 标识的错误，提示 LLM/用户。
    ///
    /// 返回 `Some(ToolResult)` 表示已识别为 denial 并生成回灌结果；
    /// 返回 `None` 表示非 denial，调用方原样传播错误。
    pub(crate) fn build_denial_result(
        detector: &dyn crate::sandbox::SandboxDenialDetector,
        breaker: &dyn crate::sandbox::SandboxDenialTracker,
        tool: &str,
        error: &crate::model::ToolError,
    ) -> Option<ToolResult> {
        let error_text = error.to_string();
        let m = detector.detect(tool, &error_text)?;
        tracing::warn!(
            tool = %m.tool,
            reason = m.signature.reason,
            platform = m.signature.platform,
            "sandbox denial detected"
        );
        let state = breaker.record_denial();
        Some(match state {
            BreakerState::HardTripped => {
                let summary = crate::sandbox::hard_trip_summary(breaker.count());
                tracing::warn!(
                    count = breaker.count(),
                    "sandbox circuit breaker hard-tripped"
                );
                metrics::set_circuit_breaker("sandbox", "hard_tripped");
                metrics::record_error("sandbox");
                ToolResult {
                    content: crate::model::ToolContent::Text(format!(
                        "{summary}\n原始错误：{error_text}"
                    )),
                    is_error: true,
                    metadata: crate::model::ToolResultMeta {
                        sandbox_denied: Some(crate::model::SandboxDenyInfo {
                            kind: m.kind.clone(),
                            detail: error_text.clone(),
                        }),
                        ..Default::default()
                    },
                }
            }
            BreakerState::SoftTripped => {
                let reminder = crate::sandbox::soft_trip_reminder(breaker.count());
                tracing::warn!(
                    count = breaker.count(),
                    "sandbox circuit breaker soft-tripped"
                );
                metrics::set_circuit_breaker("sandbox", "soft_tripped");
                metrics::record_error("sandbox");
                ToolResult {
                    content: crate::model::ToolContent::Text(format!(
                        "沙箱拒绝（{reason}）：{error_text}\n\n{reminder}",
                        reason = m.signature.reason
                    )),
                    is_error: true,
                    metadata: crate::model::ToolResultMeta {
                        sandbox_denied: Some(crate::model::SandboxDenyInfo {
                            kind: m.kind.clone(),
                            detail: error_text.clone(),
                        }),
                        ..Default::default()
                    },
                }
            }
            BreakerState::Closed => {
                metrics::record_error("sandbox");
                let mut result = ToolResult::err_text(format!(
                    "sandbox denied ({reason}): {error_text}\n\
                     提示：可切换更宽松的沙箱预设（如 --sandbox workspace-write）重试",
                    reason = m.signature.reason
                ));
                result.metadata.sandbox_denied = Some(crate::model::SandboxDenyInfo {
                    kind: m.kind,
                    detail: error_text,
                });
                result
            }
        })
    }
}
