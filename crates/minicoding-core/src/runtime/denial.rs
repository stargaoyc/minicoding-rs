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

/// S-6：从工具错误中提取结构化沙箱 errno。
///
/// 仅匹配 `ToolError::Io` 且 OS error code 为 `EPERM`(1)/`EACCES`(13)——这是
/// 内核级硬反馈的可靠信号。子进程 stderr **文本**可被业务失败或提示注入间接
/// 控制，不可作为熔断依据；结构化 errno 由 Rust 进程内的 `io::Error` 携带，
/// LLM 无法伪造。
fn structured_denial_errno(error: &crate::model::ToolError) -> Option<i32> {
    match error {
        crate::model::ToolError::Io(e) => match e.raw_os_error() {
            Some(code @ (ERRNO_EPERM | ERRNO_EACCES)) => Some(code),
            _ => None,
        },
        _ => None,
    }
}

/// `EPERM`（unix errno 1，操作不允许）。
///
/// 平台差异（2026-08-25 第三次 v0.3.4 CI 教训）：Windows 的 raw OS error code
/// 体系不同——`raw_os_error(1)` 在 Windows 是 `ERROR_INVALID_FUNCTION`，权限
/// 拒绝是 code 5。权威判定按平台取正确编码。
#[cfg(not(target_os = "windows"))]
const ERRNO_EPERM: i32 = 1;
/// Windows `ERROR_ACCESS_DENIED`（5）。
#[cfg(target_os = "windows")]
const ERRNO_EPERM: i32 = 5;

/// `EACCES`（unix errno 13，权限不足）。
const ERRNO_EACCES: i32 = 13;

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
    /// 见 [`Self::build_denial_result`]：权威判定（结构化 errno 标记命中）
    /// 更新熔断器并按软/硬阈值处理；advisory 命中仅返回提示性结果。
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
    /// 检测工具错误是否为沙箱拒绝。错误携带结构化 errno（EPERM/EACCES，S-6）
    /// 时向检测文本追加 Runtime 合成的权威标记行；检测结果分两路：
    /// - **authoritative**（标记命中）：内核级硬反馈——更新熔断器（C-30），
    ///   软/硬熔断分支仅在此路径可达；
    /// - **advisory**（仅文本模式命中）：疑似拒绝但未经结构化确认——返回提示性
    ///   `ToolResult` 但不计熔断、不动 `set_circuit_breaker` 指标（日志与
    ///   metrics 类目标注 advisory）。
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
        // S-6：组装检测文本——结构化 errno 存在时追加合成标记行。子进程输出
        // 无法伪造 `\x01`/`\x02` 控制字符序列，且标记由 Runtime 进程内合成，
        // 据此区分权威判定与 advisory 命中。回灌 LLM 的文本保持原始错误
        // （不带标记），避免控制字符进入上下文。
        let detect_text = match structured_denial_errno(error) {
            Some(errno) => format!(
                "{error_text}\n{prefix}{errno}{suffix}",
                prefix = crate::sandbox::DENIED_ERRNO_MARKER_PREFIX,
                suffix = crate::sandbox::DENIED_ERRNO_MARKER_SUFFIX
            ),
            None => error_text.clone(),
        };
        let m = detector.detect(tool, &detect_text)?;
        if !m.authoritative {
            // advisory：文本启发式命中，可能来自业务失败或伪造输出——不计熔断
            tracing::info!(
                tool = %m.tool,
                reason = m.signature.reason,
                platform = m.signature.platform,
                "sandbox denial signature matched (advisory, not counted)"
            );
            metrics::record_error("sandbox_advisory");
            let mut result = ToolResult::err_text(format!(
                "sandbox denied ({reason}) [advisory]: {error_text}\n\
                 提示：该错误疑似沙箱拒绝但未经内核级确认，未计入熔断计数",
                reason = m.signature.reason
            ));
            result.metadata.sandbox_denied = Some(crate::model::SandboxDenyInfo {
                kind: m.kind,
                detail: error_text,
            });
            return Some(result);
        }
        tracing::warn!(
            tool = %m.tool,
            reason = m.signature.reason,
            platform = m.signature.platform,
            authoritative = true,
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

#[cfg(test)]
mod tests {
    //! S-6 回归测试：结构化 errno → authoritative（计入熔断）；
    //! 纯文本命中 → advisory（不计熔断）。软/硬熔断分支仅权威路径可达。

    use super::*;
    use crate::sandbox::{
        DENIED_ERRNO_MARKER_PREFIX, DenialMatch, DenialSignature, NoopDenialTracker,
        SandboxDenialDetector, SandboxDenialTracker, SandboxDenyKind,
    };

    /// 测试检测器：模拟 `minicoding-sandbox` 的真实语义——文本模式命中返回
    /// advisory，检测文本含 Runtime 合成标记时置 authoritative。
    struct FakeDetector;

    impl SandboxDenialDetector for FakeDetector {
        fn detect(&self, tool: &str, error_text: &str) -> Option<DenialMatch> {
            const PATTERN: &str = "Operation not permitted";
            // 权威路径：Runtime 合成的内部 errno 标记（平台无关——Windows 上
            // io::Error Display 不是 "Operation not permitted"，此前按 OS 文案
            // 匹配导致 windows runner 测试失败）
            let authoritative = error_text.contains(crate::sandbox::DENIED_ERRNO_MARKER_PREFIX);
            if !authoritative && !error_text.contains(PATTERN) {
                return None;
            }
            Some(DenialMatch {
                signature: DenialSignature {
                    platform: "any",
                    pattern: PATTERN,
                    reason: "EPERM",
                    kind_label: "syscall_blocked",
                },
                tool: tool.to_string(),
                kind: SandboxDenyKind::SyscallBlocked {
                    syscall: PATTERN.to_string(),
                },
                authoritative,
            })
        }
    }

    fn eperm_io_error() -> crate::model::ToolError {
        crate::model::ToolError::Io(std::io::Error::from_raw_os_error(ERRNO_EPERM))
    }

    /// 提取结果文本（测试辅助）。
    fn text_of(result: &ToolResult) -> String {
        match &result.content {
            crate::model::ToolContent::Text(t) => t.clone(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn structured_errno_matches_io_eperm_and_eacces() {
        // 平台无关断言：与常量比较而非字面量（Windows 上 EPERM=5 非 unix 的 1）
        assert_eq!(
            structured_denial_errno(&eperm_io_error()),
            Some(ERRNO_EPERM)
        );
        assert_eq!(
            structured_denial_errno(&crate::model::ToolError::Io(
                std::io::Error::from_raw_os_error(ERRNO_EACCES)
            )),
            Some(ERRNO_EACCES)
        );
        // 非 deny-list errno 不构成权威信号
        assert_eq!(
            structured_denial_errno(&crate::model::ToolError::Io(
                std::io::Error::from_raw_os_error(0)
            )),
            None
        );
    }

    #[test]
    fn structured_errno_ignores_non_sandbox_sources() {
        // 文本类错误无 errno——LLM 可间接影响的通道不得作为熔断依据
        assert_eq!(
            structured_denial_errno(&crate::model::ToolError::Exec(
                "Operation not permitted".into()
            )),
            None
        );
        // 其他 errno（如 ENOENT=2）不是沙箱拒绝信号
        assert_eq!(
            structured_denial_errno(&crate::model::ToolError::Io(
                std::io::Error::from_raw_os_error(2)
            )),
            None
        );
        assert_eq!(
            structured_denial_errno(&crate::model::ToolError::Cancelled),
            None
        );
    }

    #[test]
    fn advisory_hit_returns_result_without_breaker_record() {
        let breaker = NoopDenialTracker::default_thresholds();
        let error = crate::model::ToolError::Exec("Operation not permitted".into());
        let result = Runtime::build_denial_result(&FakeDetector, &breaker, "shell.run", &error)
            .expect("纯文本命中应产出提示性结果");
        assert!(result.is_error);
        let text = text_of(&result);
        assert!(
            text.contains("[advisory]"),
            "advisory 结果应标注 [advisory]，实际: {text}"
        );
        assert!(
            breaker.count() == 0 && matches!(breaker.state(), BreakerState::Closed),
            "advisory 命中不得计入熔断"
        );
        // 结构化透传仍保留（前端渲染卡片），detail 为原始错误（不含标记）
        let info = result.metadata.sandbox_denied.expect("metadata 透传");
        assert!(!info.detail.contains(DENIED_ERRNO_MARKER_PREFIX));
    }

    #[test]
    fn authoritative_eperm_records_breaker() {
        let breaker = NoopDenialTracker::default_thresholds();
        let error = eperm_io_error();
        let result = Runtime::build_denial_result(&FakeDetector, &breaker, "fs.write", &error)
            .expect("Io(EPERM) 应判定为 denial");
        assert!(result.is_error);
        assert_eq!(breaker.count(), 1, "authoritative 命中应递增熔断计数");
    }

    #[test]
    fn soft_hard_trip_only_reachable_via_authoritative_path() {
        // soft=2/hard=3：第 1 次权威拒绝 Closed、第 2 次软熔断、第 3 次硬熔断
        let breaker = NoopDenialTracker::new(2, 3);
        let first =
            Runtime::build_denial_result(&FakeDetector, &breaker, "shell.run", &eperm_io_error())
                .unwrap();
        assert_eq!(breaker.count(), 1);
        assert!(
            !text_of(&first).contains("连续"),
            "Closed 态不应带软熔断提醒，实际: {}",
            text_of(&first)
        );
        let second =
            Runtime::build_denial_result(&FakeDetector, &breaker, "shell.run", &eperm_io_error())
                .unwrap();
        assert_eq!(breaker.count(), 2);
        assert!(
            text_of(&second).contains("连续"),
            "软熔断提醒应回灌，实际: {}",
            text_of(&second)
        );
        let third =
            Runtime::build_denial_result(&FakeDetector, &breaker, "shell.run", &eperm_io_error())
                .unwrap();
        assert!(matches!(breaker.state(), BreakerState::HardTripped));
        assert!(
            text_of(&third).contains("熔断"),
            "硬熔断总结应回灌，实际: {}",
            text_of(&third)
        );

        // advisory 路径在已 HardTripped 时仍不改变计数/状态（分支不可达验证）
        let before = breaker.count();
        let error = crate::model::ToolError::Exec("Operation not permitted".into());
        let advisory =
            Runtime::build_denial_result(&FakeDetector, &breaker, "shell.run", &error).unwrap();
        assert!(text_of(&advisory).contains("[advisory]"));
        assert_eq!(breaker.count(), before);
        assert!(matches!(breaker.state(), BreakerState::HardTripped));
    }

    #[test]
    fn non_denial_errors_propagate_none() {
        let breaker = NoopDenialTracker::default_thresholds();
        let error = crate::model::ToolError::Exec("file not found".into());
        assert!(
            Runtime::build_denial_result(&FakeDetector, &breaker, "shell.run", &error).is_none(),
            "未命中签名应返回 None（原样传播错误）"
        );
    }
}
