//! `ExtensionManifest` + `ExtensionCarrier` + `Capability` + 6 类注册项数据结构。
//!
//! 扩展通过 manifest 声明 id/version/capabilities/permissions，`ExtensionHost` 加载
//! 时静态校验。manifest 是扩展作者与 Runtime 之间的契约，未来 manifest schema 演进
//! 通过 `version` 字段做兼容性判断。

use camino::Utf8PathBuf;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::fmt;

/// 扩展全局唯一 id（如 `minicoding-git-stats`）。
///
/// 用 newtype 封装 `String` 而非裸 `String`，避免与其他字符串参数混淆。id 命名规约：
/// `^[a-z][a-z0-9-]*$`（小写字母/数字/连字符，与 npm/cargo 包名风格一致）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExtensionId(pub String);

impl fmt::Display for ExtensionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ExtensionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ExtensionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for ExtensionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// 扩展清单（声明扩展元信息与能力边界）。
///
/// `ExtensionHost::load_extension` 加载时校验：
/// 1. `id` 唯一（未重复加载）；
/// 2. `capabilities` 与 `Registrar::register_*` 实际注册项匹配（多声明允许，少声明
///    报 `CapabilityNotDeclared`）；
/// 3. `permissions` 经 `PermissionPolicy` 静态校验（C-01）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    /// 全局唯一 id。
    pub id: ExtensionId,
    /// 语义化版本（兼容性判断用）。
    pub version: Version,
    /// 人类可读名（展示用，不唯一）。
    pub name: String,
    /// 作者（可选）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// 扩展载体（决定加载方式）。
    pub carrier: ExtensionCarrier,
    /// 声明的能力（与 `Registrar` 6 个方法一一对应）。
    pub capabilities: Vec<Capability>,
    /// 申请的权限范围（如 `["fs.read", "shell.run:git *"]`）。
    #[serde(default)]
    pub permissions: Vec<Permission>,
    /// 配置 JSON Schema（可选，校验 `[extension.<id>]` 配置段）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
}

/// 扩展载体（三类统一抽象，见 §23 表格）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExtensionCarrier {
    /// 进程内 first-party：`minicoding-extension-sdk` 通过 name 查找符号加载。
    Bundled,
    /// disk IPC 子进程：`minicoding-cli` 加载器启动可执行文件，JSON over stdio 通信。
    Ipc { path: Utf8PathBuf },
    /// MCP 远程扩展：复用 `minicoding-mcp` 的 `McpClient`，通过 `server_id` 关联。
    Mcp { server_id: String },
}

/// 扩展能力声明（与 `Registrar` 6 个方法一一对应）。
///
/// 扩展在 manifest 中声明此列表，`ExtensionHost` 据此校验 `register_*` 调用是否越界。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// 注册工具（`Registrar::register_tool`）。
    Tool,
    /// 注册 Hook（`Registrar::register_hook`）。
    Hook,
    /// 注册 prompt contributor（`Registrar::register_prompt_contributor`）。
    PromptContributor,
    /// 注册快捷键（`Registrar::register_keybinding`）。
    Keybinding,
    /// 注册状态栏项（`Registrar::register_status_item`）。
    StatusItem,
    /// 注册斜杠命令（`Registrar::register_command`）。
    Command,
}

impl Capability {
    /// 转为 `Registrar` 方法名字符串（错误信息用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Hook => "hook",
            Self::PromptContributor => "prompt_contributor",
            Self::Keybinding => "keybinding",
            Self::StatusItem => "status_item",
            Self::Command => "command",
        }
    }
}

/// 扩展申请的权限（字符串格式，运行时由 `PermissionPolicy` 解析）。
///
/// 格式参考 `rules.md` 的权限规则：
/// - 工具级：`"fs.read"`、`"shell.run"`；
/// - 工具+参数级：`"shell.run:git *"`（通配）；
/// - 资源级（未来）：`"fs.read:/tmp/*"`。
///
/// `ExtensionHost::load_extension` 把 `permissions` 列表传给 `PermissionPolicy::check`
/// 做静态校验，任一被 `Deny` 则加载失败（`ExtensionError::PermissionDenied`）。
pub type Permission = String;

/// 已加载扩展的运行时信息（`ExtensionHost::list_extensions` 返回）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionInfo {
    /// 扩展 id。
    pub id: ExtensionId,
    /// 版本（manifest 的副本）。
    pub version: Version,
    /// 人类可读名。
    pub name: String,
    /// 载体类型。
    pub carrier: ExtensionCarrier,
    /// 已注册的能力数（按 `Capability` 分类计数）。
    pub registered: Vec<(Capability, usize)>,
}

/// 快捷键绑定（`Registrar::register_keybinding` 用）。
///
/// 仅 TUI 前端消费，CLI/SDK 前端忽略。键序列用 crossterm 的 `KeyEvent` 序列化格式。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyBinding {
    /// 键序列（如 `"Ctrl+P"`、`"Alt+Enter"`，crossterm 格式）。
    pub key: String,
    /// 触发的命令名（关联 `SlashCommand` 或内置动作）。
    pub command: String,
    /// 描述（UI 提示用）。
    pub description: String,
}

/// 状态栏项（`Registrar::register_status_item` 用）。
///
/// TUI 状态栏展示，如 git 分支、token 用量、当前 plan 模式等。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusItem {
    /// 唯一 id（同扩展内唯一）。
    pub id: String,
    /// 展示优先级（数字越小越靠左，默认 100）。
    pub priority: u32,
    /// 初始文本。
    pub text: String,
}

/// 斜杠命令（`Registrar::register_command` 用）。
///
/// 用户在交互会话中输入 `/<name>` 触发。命令实现为闭包（由扩展注入），Runtime 不
/// 关心具体逻辑，只负责把用户输入与参数投递给扩展并接收结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommand {
    /// 命令名（不带 `/`，如 `"stats"` 对应 `/stats`）。
    pub name: String,
    /// 简短描述（`/help` 列表用）。
    pub description: String,
    /// 参数 schema（JSON Schema，可空表示无参数）。
    #[serde(default)]
    pub args_schema: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn extension_id_display_and_from() {
        let id = ExtensionId::from("minicoding-git-stats");
        assert_eq!(id.as_ref(), "minicoding-git-stats");
        assert_eq!(id.to_string(), "minicoding-git-stats");
        let id2: ExtensionId = "test".into();
        assert_eq!(id2.0, "test");
    }

    #[test]
    fn manifest_serde_roundtrip() {
        let m = ExtensionManifest {
            id: ExtensionId("test-ext".into()),
            version: Version::new(0, 1, 0),
            name: "Test Extension".into(),
            author: Some("tester".into()),
            carrier: ExtensionCarrier::Bundled,
            capabilities: vec![Capability::Tool, Capability::Command],
            permissions: vec!["fs.read".into()],
            config_schema: None,
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let back: ExtensionManifest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, m.id);
        assert_eq!(back.version, m.version);
        assert_eq!(back.capabilities, m.capabilities);
        assert!(matches!(back.carrier, ExtensionCarrier::Bundled));
    }

    #[test]
    fn carrier_ipc_serde() {
        let m = ExtensionManifest {
            id: ExtensionId("ipc-ext".into()),
            version: Version::new(1, 0, 0),
            name: "IPC".into(),
            author: None,
            carrier: ExtensionCarrier::Ipc {
                path: Utf8PathBuf::from("/usr/local/bin/my-ext"),
            },
            capabilities: vec![Capability::Hook],
            permissions: Vec::new(),
            config_schema: None,
        };
        let json = serde_json::to_string(&m).expect("serialize");
        assert!(json.contains("\"kind\":\"ipc\""));
        assert!(json.contains("/usr/local/bin/my-ext"));
    }

    #[test]
    fn capability_as_str_covers_all_variants() {
        assert_eq!(Capability::Tool.as_str(), "tool");
        assert_eq!(Capability::Hook.as_str(), "hook");
        assert_eq!(Capability::PromptContributor.as_str(), "prompt_contributor");
        assert_eq!(Capability::Keybinding.as_str(), "keybinding");
        assert_eq!(Capability::StatusItem.as_str(), "status_item");
        assert_eq!(Capability::Command.as_str(), "command");
    }
}
