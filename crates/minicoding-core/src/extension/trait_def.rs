//! `Extension` trait + `ExtensionHost` trait + `Registrar` trait + `NoopExtensionHost`。
//!
//! trait 定义集中在 core（§3.3），实现在 `minicoding-extension-sdk`（进程内 first-party）
//! 或 `minicoding-cli`（disk IPC 加载器）。Runtime 持有 `Arc<dyn ExtensionHost>` 不
//! 感知具体实现 crate。
//!
//! 与 `Hook`/`LlmProvider` 一致，异步方法用 `BoxFuture` 返回类型保证 `dyn` 兼容
//! （`async fn in trait` 的 `dyn` 兼容需 boxed future，且默认实现需要 boxed）。

use crate::extension::manifest::{
    Capability, ExtensionId, ExtensionInfo, ExtensionManifest, KeyBinding, SlashCommand, StatusItem,
};
use crate::hooks::Hook;
use crate::model::ExtensionError;
use crate::prompt::PromptContributor;
use crate::provider::BoxFuture;
use crate::tool::Tool;
use std::sync::Arc;

/// 扩展 trait（扩展作者实现，见 `api.md` §3.12）。
///
/// 生命周期：`ExtensionHost::load_extension` → `init`（注册能力）→ 运行期 →
/// `ExtensionHost::unload_extension` → `shutdown`（释放资源）。
///
/// `init` 接收 `&mut dyn Registrar`，扩展通过它注册 6 类能力。`manifest()` 让
/// Runtime 无需维护独立的扩展元信息表，直接从扩展实例查询。
///
/// 与 `Hook`/`LlmProvider` 一致，异步方法用 `BoxFuture` 返回类型保证 `dyn` 兼容。
pub trait Extension: Send + Sync {
    /// 扩展元信息。
    fn manifest(&self) -> &ExtensionManifest;

    /// 初始化：注册能力、订阅事件、读配置。
    ///
    /// `config` 为 `[extension.<id>]` 配置段（JSON），已据 `config_schema` 校验。
    /// 失败返回 `ExtensionError::InitFailed`，扩展不会注册。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::InitFailed`] 当注册能力校验失败或配置非法。
    fn init(
        &self,
        registrar: &mut dyn Registrar,
        config: serde_json::Value,
    ) -> BoxFuture<'_, Result<(), ExtensionError>>;

    /// 卸载：释放资源、取消订阅。
    ///
    /// 由 `ExtensionHost::unload_extension` 调用。失败返回
    /// `ExtensionError::ShutdownFailed`，但扩展仍会被注销（best-effort）。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::ShutdownFailed`] 当资源释放异常。
    fn shutdown(&self) -> BoxFuture<'_, Result<(), ExtensionError>>;

    /// 配置变更通知（可选，默认空实现）。
    ///
    /// `ConfigWatcher` 检测到配置文件变化时由 `ExtensionHost::on_config_changed` 投递。
    ///
    /// # Errors
    /// 由实现者定义，默认实现返回 `Ok(())`。
    fn on_config_changed(
        &self,
        _new_config: serde_json::Value,
    ) -> BoxFuture<'_, Result<(), ExtensionError>> {
        Box::pin(async move { Ok(()) })
    }
}

/// 注册器：扩展通过此接口注册 6 类能力（见 `api.md` §3.12）。
///
/// `ExtensionHost::load_extension` 在调用 `Extension::init` 时构造一个 `Registrar`
/// 实例传入。扩展的 `register_*` 调用被记录，init 成功后批量提交到 Runtime 各注册表
/// （`ToolRegistry`/`HookRegistry`/`PromptPipeline` 等）。
///
/// **能力校验**：每次 `register_*` 检查 manifest 是否声明了对应 `Capability`，
/// 未声明返回 `ExtensionError::CapabilityNotDeclared`。
pub trait Registrar {
    /// 注册工具（仍走 `ToolRegistry::dispatch`，C-01/C-02 不被绕过）。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::CapabilityNotDeclared`] 当 manifest 未声明 `Capability::Tool`。
    fn register_tool(&mut self, tool: Arc<dyn Tool>) -> Result<(), ExtensionError>;

    /// 注册 Hook（仍受 L0 优先约束，C-21）。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::CapabilityNotDeclared`] 当 manifest 未声明 `Capability::Hook`。
    fn register_hook(&mut self, hook: Arc<dyn Hook>) -> Result<(), ExtensionError>;

    /// 注册 prompt contributor（注入到 `PromptPipeline` 的 `Extension` 段，顺序 9）。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::CapabilityNotDeclared`] 当 manifest 未声明 `Capability::PromptContributor`。
    fn register_prompt_contributor(
        &mut self,
        contributor: Arc<dyn PromptContributor>,
    ) -> Result<(), ExtensionError>;

    /// 注册快捷键（TUI 前端消费）。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::CapabilityNotDeclared`] 当 manifest 未声明 `Capability::Keybinding`。
    fn register_keybinding(&mut self, kb: KeyBinding) -> Result<(), ExtensionError>;

    /// 注册状态栏项（TUI 前端消费）。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::CapabilityNotDeclared`] 当 manifest 未声明 `Capability::StatusItem`。
    fn register_status_item(&mut self, item: StatusItem) -> Result<(), ExtensionError>;

    /// 注册斜杠命令（`/<name>` 触发）。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::CapabilityNotDeclared`] 当 manifest 未声明 `Capability::Command`。
    fn register_command(&mut self, cmd: SlashCommand) -> Result<(), ExtensionError>;
}

/// 扩展宿主：管理扩展生命周期（Runtime 注入，见 `api.md` §3.12）。
///
/// Runtime 启动时调用 `load_extension` 批量加载 `~/.minicoding/extensions/` 下的
/// 扩展；运行期可 `unload_extension`/`on_config_changed`；关闭时调用所有扩展的
/// `shutdown`。
///
/// 异步方法用 `BoxFuture` 返回类型保证 `dyn` 兼容。
pub trait ExtensionHost: Send + Sync {
    /// 加载扩展（读 manifest，初始化，注册能力）。
    ///
    /// 校验：id 唯一、capabilities 与注册项匹配、permissions 经 `PermissionPolicy`
    /// 静态校验通过。任一失败返回 `ExtensionError`，扩展未注册。
    ///
    /// # Errors
    /// - [`ExtensionError::AlreadyLoaded`]：id 重复；
    /// - [`ExtensionError::InitFailed`]：扩展 `init` 失败；
    /// - [`ExtensionError::PermissionDenied`]：权限静态校验未通过；
    /// - [`ExtensionError::InvalidManifest`]：manifest 非法。
    fn load_extension(
        &self,
        manifest: ExtensionManifest,
    ) -> BoxFuture<'_, Result<ExtensionId, ExtensionError>>;

    /// 卸载扩展（调用 shutdown，注销所有注册项）。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::NotFound`] 当 id 不存在；[`ExtensionError::ShutdownFailed`] 当 shutdown 失败。
    fn unload_extension(&self, id: &ExtensionId) -> BoxFuture<'_, Result<(), ExtensionError>>;

    /// 列出已加载扩展。
    fn list_extensions(&self) -> BoxFuture<'_, Vec<ExtensionInfo>>;

    /// 配置变更通知（热重载，按扩展 id 投递）。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::NotFound`] 当 id 不存在。
    fn on_config_changed(
        &self,
        id: &ExtensionId,
        new_config: serde_json::Value,
    ) -> BoxFuture<'_, Result<(), ExtensionError>>;
}

/// 默认兜底：未启用扩展时使用。
///
/// `load_extension` 恒返回 `NotFound`（不支持运行时加载），`list_extensions` 返回
/// 空列表。Runtime 默认注入此实现，启用扩展时由 frontend 注入
/// `BundledExtensionHost`（`minicoding-extension-sdk`）。
#[derive(Debug, Default, Clone)]
pub struct NoopExtensionHost;

impl NoopExtensionHost {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ExtensionHost for NoopExtensionHost {
    fn load_extension(
        &self,
        _manifest: ExtensionManifest,
    ) -> BoxFuture<'_, Result<ExtensionId, ExtensionError>> {
        Box::pin(async move {
            Err(ExtensionError::NotFound(
                "extension host not configured (NoopExtensionHost)".into(),
            ))
        })
    }

    fn unload_extension(&self, id: &ExtensionId) -> BoxFuture<'_, Result<(), ExtensionError>> {
        let id = id.clone();
        Box::pin(async move {
            Err(ExtensionError::NotFound(format!(
                "{id}: extension host not configured (NoopExtensionHost)"
            )))
        })
    }

    fn list_extensions(&self) -> BoxFuture<'_, Vec<ExtensionInfo>> {
        Box::pin(async move { Vec::new() })
    }

    fn on_config_changed(
        &self,
        id: &ExtensionId,
        _new_config: serde_json::Value,
    ) -> BoxFuture<'_, Result<(), ExtensionError>> {
        let id = id.clone();
        Box::pin(async move {
            Err(ExtensionError::NotFound(format!(
                "{id}: extension host not configured (NoopExtensionHost)"
            )))
        })
    }
}

/// `Registrar` 的 noop 实现（无 manifest 校验时使用，如测试）。
///
/// 注册项全部丢弃，仅做 `Capability` 声明校验。用于未集成真实 `ExtensionHost` 的
/// 测试场景或 `NoopExtensionHost`。
#[derive(Debug, Default)]
pub struct NoopRegistrar {
    capabilities: Vec<Capability>,
}

impl NoopRegistrar {
    /// 创建 noop registrar，传入 manifest 声明的 capabilities 用于校验。
    #[must_use]
    pub fn new(capabilities: Vec<Capability>) -> Self {
        Self { capabilities }
    }

    fn check(&self, cap: Capability) -> Result<(), ExtensionError> {
        if self.capabilities.contains(&cap) {
            Ok(())
        } else {
            Err(ExtensionError::CapabilityNotDeclared(cap.as_str().into()))
        }
    }
}

impl Registrar for NoopRegistrar {
    fn register_tool(&mut self, _tool: Arc<dyn Tool>) -> Result<(), ExtensionError> {
        self.check(Capability::Tool)
    }

    fn register_hook(&mut self, _hook: Arc<dyn Hook>) -> Result<(), ExtensionError> {
        self.check(Capability::Hook)
    }

    fn register_prompt_contributor(
        &mut self,
        _contributor: Arc<dyn PromptContributor>,
    ) -> Result<(), ExtensionError> {
        self.check(Capability::PromptContributor)
    }

    fn register_keybinding(&mut self, _kb: KeyBinding) -> Result<(), ExtensionError> {
        self.check(Capability::Keybinding)
    }

    fn register_status_item(&mut self, _item: StatusItem) -> Result<(), ExtensionError> {
        self.check(Capability::StatusItem)
    }

    fn register_command(&mut self, _cmd: SlashCommand) -> Result<(), ExtensionError> {
        self.check(Capability::Command)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use crate::extension::manifest::ExtensionCarrier;
    use semver::Version;

    #[tokio::test]
    async fn noop_host_load_returns_not_found() {
        let host = NoopExtensionHost::new();
        let manifest = ExtensionManifest {
            id: ExtensionId("test".into()),
            version: Version::new(0, 1, 0),
            name: "Test".into(),
            author: None,
            carrier: ExtensionCarrier::Bundled,
            capabilities: Vec::new(),
            permissions: Vec::new(),
            config_schema: None,
        };
        let result = host.load_extension(manifest).await;
        assert!(matches!(result, Err(ExtensionError::NotFound(_))));
    }

    #[tokio::test]
    async fn noop_host_list_returns_empty() {
        let host = NoopExtensionHost::new();
        assert!(host.list_extensions().await.is_empty());
    }

    #[tokio::test]
    async fn noop_registrar_rejects_undeclared_capability() {
        let r = NoopRegistrar::new(vec![]);
        // Tool 能力未声明，注册应失败
        let result = r.check(Capability::Tool);
        assert!(matches!(
            result,
            Err(ExtensionError::CapabilityNotDeclared(_))
        ));
    }

    #[tokio::test]
    async fn noop_registrar_accepts_declared_capability() {
        let r = NoopRegistrar::new(vec![Capability::Tool]);
        let result = r.check(Capability::Tool);
        assert!(result.is_ok());
    }
}
