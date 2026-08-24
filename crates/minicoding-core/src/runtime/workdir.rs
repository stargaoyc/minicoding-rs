//! 工作区切换（A-2026-08 自 rt.rs 抽出；W-11，见 `design.md` §20）。
//!
//! `switch_workdir` 是唯一在工具执行链之外改变副作用作用域的入口，
//! 独立成文以隔离其"校验 → 弹窗 → 审计 → 生效"的完整因果链。

use super::Event;
use super::rt::Runtime;
use crate::model::RuntimeError;
use crate::policy::{Decision, PermissionPrompt};
use camino::Utf8PathBuf;

impl Runtime {
    /// 切换工作目录（W-11 工作区切换，需用户显式批准，Ask 级权限）。
    ///
    /// 流程（与副作用工具权限路径一致，见 `design.md` §9）：
    /// 1. 校验 `target` 为绝对路径 + 目标**真实存在且是目录**（`canonicalize`）——
    ///    校验在弹权限窗前完成，目录不存在/不可访问时立即报错（避免用户等待
    ///    审批后才在后续浏览中到处 404）；
    /// 2. 构造 `PermissionPrompt`（`tool: "workspace.switch"`，`Risk::Medium`，
    ///    仅 `AllowOnce`/`DenyOnce`——不允许 `AllowAlways`，切换必须逐次确认）；
    /// 3. 广播 `Event::PermissionRequested`（SSE 推送到前端弹窗，复用 W-03 权限
    ///    弹窗机制）→ `prompter.prompt` 等待决策；
    /// 4. 广播 + 持久化 `Event::PermissionResolved`，落 `audit.log`（C-01 语义：
    ///    工作区切换改变后续所有副作用工具的作用范围，等同副作用决策）；
    /// 5. `Allow` → 更新 `workdir` 为 canonicalize 后的规范化路径（后续工具调用
    ///    自动生效，C-03 跟随新 root）；`Deny` → 保持原目录。
    ///
    /// 调用方（HTTP `POST /sessions/{id}/workspace`）应在持有 turn 锁时调用，
    /// 避免与进行中的 turn 交错（本方法在 Runtime 内不自行加锁）。
    ///
    /// # Errors
    /// `target` 非绝对路径、目录不存在或不可访问时返回 `RuntimeError::Permission`；
    /// 存储持久化失败时返回 `RuntimeError::Storage`。
    ///
    /// # Returns
    /// `true` = 切换成功；`false` = 用户拒绝。
    pub async fn switch_workdir(&self, target: &Utf8PathBuf) -> Result<bool, RuntimeError> {
        if !target.is_absolute() {
            return Err(RuntimeError::Permission(
                "workspace.switch: 目标路径必须是绝对路径".to_string(),
            ));
        }
        // 目标必须真实存在且是目录：canonicalize 失败（不存在/权限不足）直接报错，
        // 不进入权限弹窗（避免"切换成功"后所有浏览 404 的假成功态）。
        let canonical = tokio::fs::canonicalize(target).await.map_err(|e| {
            RuntimeError::Permission(format!(
                "workspace.switch: 目标目录不存在或不可访问 `{target}`: {e}"
            ))
        })?;
        let meta = tokio::fs::metadata(&canonical).await.map_err(|e| {
            RuntimeError::Permission(format!(
                "workspace.switch: 无法读取目标目录 `{target}`: {e}"
            ))
        })?;
        if !meta.is_dir() {
            return Err(RuntimeError::Permission(format!(
                "workspace.switch: 目标不是目录 `{target}`"
            )));
        }
        // canonicalize 保证统一规范化（Windows 盘符/UNC/尾斜杠）；camino 类型保证 UTF-8
        let canonical = Utf8PathBuf::from_path_buf(canonical).map_err(|_| {
            RuntimeError::Permission(format!("workspace.switch: 目标路径非 UTF-8 `{target}`"))
        })?;
        // 权限弹窗展示 canonical 后的路径（用户看到的是真实目标，而非带 `..` 的原始输入）
        let target_display = canonical.as_str();

        let prompt = PermissionPrompt {
            id: format!("ws-{}", ulid::Ulid::new()),
            tool: "workspace.switch".to_string(),
            summary: format!("切换工作区到 {target_display}"),
            risk: crate::policy::Risk::Medium,
            options: vec![crate::policy::PromptOption::AllowOnce],
        };

        self.events.emit(Event::PermissionRequested {
            id: prompt.id.clone(),
            tool: prompt.tool.clone(),
            summary: prompt.summary.clone(),
            risk: prompt.risk,
        });
        let decision = self.prompter.prompt(prompt.clone()).await;
        let prompt_id = prompt.id.clone();
        let event = Event::PermissionResolved {
            id: prompt_id.clone(),
            decision: decision.clone(),
        };
        self.persist_event(&event).await;
        self.events.emit(event);
        self.record_permission_audit("workspace.switch", &decision, Some(prompt_id), None)
            .await;

        match decision {
            Decision::Allow | Decision::AllowAlways => {
                *self.workdir.write().await = canonical.clone();
                tracing::info!(
                    session = %self.session.id,
                    workdir = %canonical,
                    "workspace switched"
                );
                Ok(true)
            }
            Decision::Deny(reason) | Decision::DenyAlways(reason) => {
                tracing::info!(
                    session = %self.session.id,
                    reason = %reason,
                    "workspace switch denied"
                );
                Ok(false)
            }
        }
    }
}
