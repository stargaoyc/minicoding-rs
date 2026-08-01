//! 审批模式（`ApprovalMode`）与预设（`Preset`）解析（T-M4-4）。
//!
//! 借鉴 Codex 的面向场景审批模式 + 预设，作为 L1 用户策略的快捷写入方式。审批模式
//! 与预设**不是独立层级**，而是展开为 specificity=1 的 L1 规则后与其他用户规则平等
//! 竞争（内置黑名单 L0 始终最高优先级，C-02）。
//!
//! ## 预设（approval mode × sandbox policy）
//!
//! | 预设 | 审批模式 | 沙箱策略 | 适用 |
//! |------|---------|---------|------|
//! | `read-only` | `OnRequest` | `ReadOnly` | 代码审计、日志诊断 |
//! | `auto`（默认） | `OnRequest` | `WorkspaceWrite` | 日常开发 |
//! | `external-sandbox` | `OnRequest` | `ExternalSandbox` | CI/容器内 |
//! | `full-access` | `Never` | `DangerFullAccess` | 受信沙箱内全自动（需 red 警告 + 二次确认，C-22） |
//!
//! 详见 `security.md` §2.6。

use camino::Utf8PathBuf;
use minicoding_core::sandbox::SandboxPolicy;
use serde::{Deserialize, Serialize};

/// 审批模式（展开为 specificity=1 的全局平移规则，见 `security.md` §2.6）。
///
/// - `Untrusted`：所有 `side_effect != None` → `Ask`；
/// - `OnFailure`：命令自动 `Allow`，失败时注入 `Ask`；
/// - `OnRequest`：默认，不写入额外规则，沿用 §2.2 矩阵；
/// - `Never`：所有 `Ask` → `Allow`（仍受 L0 黑名单与高 specificity deny 约束）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Untrusted,
    OnFailure,
    #[default]
    OnRequest,
    Never,
}

/// 预设（`approval mode × sandbox policy` 的实用组合，一键选定）。
///
/// `full-access` 展开为 `DangerFullAccess`，启动时强制 red 警告 + 二次确认（C-22）。
/// 其余预设直接展开为对应组合。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    ReadOnly,
    #[default]
    Auto,
    ExternalSandbox,
    FullAccess,
}

impl Preset {
    /// 从字符串解析预设（CLI `--preset` 用）。
    ///
    /// 接受 `kebab-case`（`read-only`/`auto`/`external-sandbox`/`full-access`）。
    ///
    /// # Errors
    /// 未知预设名时返回 `PolicyError`。
    pub fn parse(s: &str) -> Result<Self, minicoding_core::model::PolicyError> {
        match s {
            "read-only" => Ok(Self::ReadOnly),
            "auto" => Ok(Self::Auto),
            "external-sandbox" => Ok(Self::ExternalSandbox),
            "full-access" => Ok(Self::FullAccess),
            other => Err(minicoding_core::model::PolicyError::Policy(format!(
                "未知预设 `{other}`，可选：read-only / auto / external-sandbox / full-access"
            ))),
        }
    }

    /// 预设名（`kebab-case`，与 CLI 参数一致）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Auto => "auto",
            Self::ExternalSandbox => "external-sandbox",
            Self::FullAccess => "full-access",
        }
    }

    /// 展开为 `(ApprovalMode, SandboxPolicy)` 组合。
    ///
    /// `workdir` 用于 `WorkspaceWrite` 策略的工作目录绑定。`Auto` 预设展开为
    /// `WorkspaceWrite { workdir, writable: [] }`（工作区内自由读写，越界/网络 Ask）。
    #[must_use]
    pub fn expand(self, workdir: Utf8PathBuf) -> (ApprovalMode, SandboxPolicy) {
        match self {
            Self::ReadOnly => (ApprovalMode::OnRequest, SandboxPolicy::ReadOnly),
            Self::Auto => (
                ApprovalMode::OnRequest,
                SandboxPolicy::WorkspaceWrite {
                    workdir,
                    writable: Vec::new(),
                },
            ),
            Self::ExternalSandbox => (ApprovalMode::OnRequest, SandboxPolicy::ExternalSandbox),
            Self::FullAccess => (ApprovalMode::Never, SandboxPolicy::DangerFullAccess),
        }
    }

    /// 是否需要启动时 red 警告 + 二次确认（C-22）。
    ///
    /// 仅 `full-access`（`DangerFullAccess`）需要——它完全放弃内核隔离。
    #[must_use]
    pub fn requires_confirmation(self) -> bool {
        matches!(self, Self::FullAccess)
    }

    /// 警告文案（`requires_confirmation` 为 true 时使用）。
    #[must_use]
    pub fn warning_text(self) -> Option<&'static str> {
        if self.requires_confirmation() {
            Some(
                "警告：full-access 预设将完全禁用内核级沙箱隔离（DangerFullAccess）。\n\
                 模型可在工作区外任意读写、执行命令、访问网络，且无需逐次审批。\n\
                 强烈建议仅在受信容器/虚拟机内使用。确认继续？[y/N]",
            )
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn parse_presets() {
        assert_eq!(Preset::parse("read-only").unwrap(), Preset::ReadOnly);
        assert_eq!(Preset::parse("auto").unwrap(), Preset::Auto);
        assert_eq!(
            Preset::parse("external-sandbox").unwrap(),
            Preset::ExternalSandbox
        );
        assert_eq!(Preset::parse("full-access").unwrap(), Preset::FullAccess);
        assert!(Preset::parse("unknown").is_err());
    }

    #[test]
    fn expand_read_only() {
        let (mode, policy) = Preset::ReadOnly.expand(".".into());
        assert_eq!(mode, ApprovalMode::OnRequest);
        assert!(matches!(policy, SandboxPolicy::ReadOnly));
    }

    #[test]
    fn expand_auto_workspace_write() {
        let (mode, policy) = Preset::Auto.expand("/tmp/work".into());
        assert_eq!(mode, ApprovalMode::OnRequest);
        match policy {
            SandboxPolicy::WorkspaceWrite { workdir, writable } => {
                assert_eq!(workdir, "/tmp/work");
                assert!(writable.is_empty());
            }
            _ => panic!("期望 WorkspaceWrite"),
        }
    }

    #[test]
    fn expand_external_sandbox() {
        let (mode, policy) = Preset::ExternalSandbox.expand(".".into());
        assert_eq!(mode, ApprovalMode::OnRequest);
        assert!(matches!(policy, SandboxPolicy::ExternalSandbox));
    }

    #[test]
    fn expand_full_access_never() {
        let (mode, policy) = Preset::FullAccess.expand(".".into());
        assert_eq!(mode, ApprovalMode::Never);
        assert!(matches!(policy, SandboxPolicy::DangerFullAccess));
    }

    #[test]
    fn only_full_access_requires_confirmation() {
        assert!(!Preset::ReadOnly.requires_confirmation());
        assert!(!Preset::Auto.requires_confirmation());
        assert!(!Preset::ExternalSandbox.requires_confirmation());
        assert!(Preset::FullAccess.requires_confirmation());
        assert!(Preset::FullAccess.warning_text().is_some());
        assert!(Preset::Auto.warning_text().is_none());
    }

    #[test]
    fn preset_str_roundtrip() {
        for p in [
            Preset::ReadOnly,
            Preset::Auto,
            Preset::ExternalSandbox,
            Preset::FullAccess,
        ] {
            assert_eq!(Preset::parse(p.as_str()).unwrap(), p);
        }
    }
}
