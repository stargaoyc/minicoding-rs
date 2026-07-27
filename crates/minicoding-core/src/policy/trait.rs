//! `PermissionPolicy` / `PermissionPrompter` trait（见 `api.md` §3.6）。
//!
//! 决策（`PermissionPolicy`）与交互（`PermissionPrompter`）分离，解决 broadcast
//! 事件总线无法承载点对点回复的架构缺陷（见 `design.md` §9.1）。
//!
//! M1 简化：不强制使用权限（单轮只读工具）。M2 完整接入。
//!
//! 手动 `Pin<Box<dyn Future + Send>>` 返回类型保证 `dyn` 兼容。

use crate::model::SideEffect;
use crate::model::{PolicyError, SessionId};
use crate::provider::BoxFuture;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

/// 策略返回的中间判定（未交互）。
#[derive(Debug, Clone)]
pub enum Verdict {
    Allow,
    Deny(String),
    Ask(PermissionPrompt),
}

/// 交互后的最终决策（不再含 Ask）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

/// 权限上下文。
#[derive(Debug, Clone)]
pub struct PermissionContext {
    pub session: SessionId,
    pub workdir: Utf8PathBuf,
    pub side_effect: SideEffect,
    pub turn: u32,
    pub history: Vec<Decision>,
}

/// 权限请求提示（点对点交互）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionPrompt {
    pub id: String,
    pub tool: String,
    pub summary: String,
    pub risk: Risk,
    pub options: Vec<PromptOption>,
}

/// 风险等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}

/// 交互选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptOption {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
}

/// 纯决策 trait（无交互、无 IO，`dyn` 兼容）。
pub trait PermissionPolicy: Send + Sync {
    fn check(
        &self,
        tool: &str,
        input: &serde_json::Value,
        ctx: &PermissionContext,
    ) -> BoxFuture<'_, Result<Verdict, PolicyError>>;
}

/// 点对点交互器（非广播，`dyn` 兼容）。由 frontend 注入实现。
pub trait PermissionPrompter: Send + Sync {
    fn prompt(&self, req: PermissionPrompt) -> BoxFuture<'_, Decision>;
}
