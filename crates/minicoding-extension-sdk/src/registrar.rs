//! `BundleRegistrar`：扩展 `init` 时使用的注册器，收集注册项到 `RegistrationBundle`。
//!
//! 设计意图（§23）：`ExtensionHost::load_extension` 调用 `Extension::init` 时构造
//! `BundleRegistrar` 传入，扩展的 `register_*` 调用被收集到 `RegistrationBundle`。
//! init 成功后，`BundledExtensionHost` 把 bundle 提交到 Runtime 各注册表；init 失败
//! 则 bundle 被丢弃（扩展未注册）。
//!
//! **能力校验**：每次 `register_*` 检查 manifest 是否声明了对应 `Capability`，
//! 未声明返回 `ExtensionError::CapabilityNotDeclared`。这是静态校验，防止扩展越权
//! 注册未声明的能力。

use minicoding_core::extension::{
    Capability, ExtensionManifest, KeyBinding, Registrar, SlashCommand, StatusItem,
};
use minicoding_core::hooks::Hook;
use minicoding_core::model::ExtensionError;
use minicoding_core::prompt::PromptContributor;
use minicoding_core::tool::Tool;
use std::sync::Arc;

/// 扩展注册项集合（`BundleRegistrar` 收集后由 `BundledExtensionHost` 提取）。
///
/// 各字段为注册顺序的列表，`BundledExtensionHost` 把它们提交到 Runtime 对应注册表。
#[derive(Default)]
pub struct RegistrationBundle {
    /// 注册的工具（提交到 `ToolRegistry`）。
    pub tools: Vec<Arc<dyn Tool>>,
    /// 注册的 Hook（提交到 `HookRegistry`）。
    pub hooks: Vec<Arc<dyn Hook>>,
    /// 注册的 prompt contributor（提交到 `PromptPipeline` 的 `Extension` 段）。
    pub prompt_contributors: Vec<Arc<dyn PromptContributor>>,
    /// 注册的快捷键（提交到 TUI `KeyBindingMap`）。
    pub keybindings: Vec<KeyBinding>,
    /// 注册的状态栏项（提交到 TUI `StatusBarRegistry`）。
    pub status_items: Vec<StatusItem>,
    /// 注册的斜杠命令（提交到 `CommandRegistry`）。
    pub commands: Vec<SlashCommand>,
}

impl RegistrationBundle {
    /// 创建空 bundle。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否没有任何注册项。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
            && self.hooks.is_empty()
            && self.prompt_contributors.is_empty()
            && self.keybindings.is_empty()
            && self.status_items.is_empty()
            && self.commands.is_empty()
    }

    /// 各类注册项数量（诊断/审计用）。
    #[must_use]
    pub fn counts_by_capability(&self) -> Vec<(Capability, usize)> {
        let mut counts = Vec::new();
        if !self.tools.is_empty() {
            counts.push((Capability::Tool, self.tools.len()));
        }
        if !self.hooks.is_empty() {
            counts.push((Capability::Hook, self.hooks.len()));
        }
        if !self.prompt_contributors.is_empty() {
            counts.push((
                Capability::PromptContributor,
                self.prompt_contributors.len(),
            ));
        }
        if !self.keybindings.is_empty() {
            counts.push((Capability::Keybinding, self.keybindings.len()));
        }
        if !self.status_items.is_empty() {
            counts.push((Capability::StatusItem, self.status_items.len()));
        }
        if !self.commands.is_empty() {
            counts.push((Capability::Command, self.commands.len()));
        }
        counts
    }
}

impl std::fmt::Debug for RegistrationBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationBundle")
            .field("tools", &self.tools.len())
            .field("hooks", &self.hooks.len())
            .field("prompt_contributors", &self.prompt_contributors.len())
            .field("keybindings", &self.keybindings.len())
            .field("status_items", &self.status_items.len())
            .field("commands", &self.commands.len())
            .finish()
    }
}

/// `BundleRegistrar`：扩展 `init` 时使用的注册器。
///
/// 持有 manifest 声明的 capabilities 用于校验，收集注册项到内部 `RegistrationBundle`。
/// `into_bundle` 提取 bundle 供 `BundledExtensionHost` 提交到 Runtime。
pub struct BundleRegistrar {
    capabilities: Vec<Capability>,
    bundle: RegistrationBundle,
}

impl BundleRegistrar {
    /// 创建 registrar，传入 manifest 声明的 capabilities 用于校验。
    #[must_use]
    pub fn new(manifest: &ExtensionManifest) -> Self {
        Self {
            capabilities: manifest.capabilities.clone(),
            bundle: RegistrationBundle::new(),
        }
    }

    /// 提取收集到的注册项 bundle。
    #[must_use]
    pub fn into_bundle(self) -> RegistrationBundle {
        self.bundle
    }

    /// 检查 manifest 是否声明了指定 capability。
    fn check(&self, cap: Capability) -> Result<(), ExtensionError> {
        if self.capabilities.contains(&cap) {
            Ok(())
        } else {
            Err(ExtensionError::CapabilityNotDeclared(cap.as_str().into()))
        }
    }
}

impl Registrar for BundleRegistrar {
    fn register_tool(&mut self, tool: Arc<dyn Tool>) -> Result<(), ExtensionError> {
        self.check(Capability::Tool)?;
        self.bundle.tools.push(tool);
        Ok(())
    }

    fn register_hook(&mut self, hook: Arc<dyn Hook>) -> Result<(), ExtensionError> {
        self.check(Capability::Hook)?;
        self.bundle.hooks.push(hook);
        Ok(())
    }

    fn register_prompt_contributor(
        &mut self,
        contributor: Arc<dyn PromptContributor>,
    ) -> Result<(), ExtensionError> {
        self.check(Capability::PromptContributor)?;
        self.bundle.prompt_contributors.push(contributor);
        Ok(())
    }

    fn register_keybinding(&mut self, kb: KeyBinding) -> Result<(), ExtensionError> {
        self.check(Capability::Keybinding)?;
        self.bundle.keybindings.push(kb);
        Ok(())
    }

    fn register_status_item(&mut self, item: StatusItem) -> Result<(), ExtensionError> {
        self.check(Capability::StatusItem)?;
        self.bundle.status_items.push(item);
        Ok(())
    }

    fn register_command(&mut self, cmd: SlashCommand) -> Result<(), ExtensionError> {
        self.check(Capability::Command)?;
        self.bundle.commands.push(cmd);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::extension::{ExtensionCarrier, ExtensionId, ExtensionManifest};
    use semver::Version;

    fn manifest_with_caps(caps: Vec<Capability>) -> ExtensionManifest {
        ExtensionManifest {
            id: ExtensionId("test".into()),
            version: Version::new(0, 1, 0),
            name: "Test".into(),
            author: None,
            carrier: ExtensionCarrier::Bundled,
            capabilities: caps,
            permissions: Vec::new(),
            config_schema: None,
        }
    }

    #[test]
    fn registrar_rejects_undeclared_capability() {
        let m = manifest_with_caps(vec![]);
        let mut r = BundleRegistrar::new(&m);
        // Tool 能力未声明，注册应失败
        let result = r.register_tool(Arc::new(StubTool));
        assert!(matches!(
            result,
            Err(ExtensionError::CapabilityNotDeclared(_))
        ));
    }

    #[test]
    fn bundle_is_initially_empty() {
        let b = RegistrationBundle::new();
        assert!(b.is_empty(), "expected empty: b");
        assert!(b.counts_by_capability().is_empty());
    }

    #[test]
    fn counts_by_capability_reflects_registrations() {
        let m = manifest_with_caps(vec![Capability::Tool, Capability::Command]);
        let mut r = BundleRegistrar::new(&m);
        r.register_tool(Arc::new(StubTool)).unwrap();
        r.register_tool(Arc::new(StubTool)).unwrap();
        r.register_command(SlashCommand {
            name: "test".into(),
            description: "test cmd".into(),
            args_schema: None,
        })
        .unwrap();
        let b = r.into_bundle();
        assert!(!b.is_empty(), "expected non-empty: b");
        let counts = b.counts_by_capability();
        assert!(counts.contains(&(Capability::Tool, 2)));
        assert!(counts.contains(&(Capability::Command, 1)));
    }

    /// 桩 Tool 用于测试注册。
    struct StubTool;
    impl Tool for StubTool {
        fn name(&self) -> &str {
            "stub"
        }
        fn schema(&self) -> &minicoding_core::model::ToolSchema {
            use minicoding_core::model::ToolSchema;
            // 测试桩用 OnceLock 持有 schema 引用（避免每次构造）。
            static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| ToolSchema {
                name: "stub".into(),
                description: "stub".into(),
                input_schema: serde_json::Value::Null,
            })
        }
        fn side_effect(&self) -> minicoding_core::model::SideEffect {
            minicoding_core::model::SideEffect::None
        }
        fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &minicoding_core::tool::ToolContext,
        ) -> minicoding_core::provider::BoxFuture<
            '_,
            Result<minicoding_core::model::ToolResult, minicoding_core::model::ToolError>,
        > {
            Box::pin(async {
                Ok(minicoding_core::model::ToolResult {
                    content: minicoding_core::model::ToolContent::Text("ok".into()),
                    is_error: false,
                    metadata: minicoding_core::model::ToolResultMeta::default(),
                })
            })
        }
    }
}
