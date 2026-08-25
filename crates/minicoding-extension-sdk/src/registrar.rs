//! `BundleRegistrar`：扩展 `init` 时使用的注册器，收集注册项到 `RegistrationBundle`。
//!
//! 设计意图（§23）：`ExtensionHost::load_extension` 调用 `Extension::init` 时构造
//! `BundleRegistrar` 传入，扩展的 `register_*` 调用被收集到 `RegistrationBundle`。
//! init 成功后，`BundledExtensionHost` 把 bundle 提交到 Runtime 各注册表；init 失败
//! 则 bundle 被丢弃（扩展未注册，事务式语义保持）。
//!
//! **能力校验**：每次 `register_*` 检查 manifest 是否声明了对应 `Capability`，
//! 未声明返回 `ExtensionError::CapabilityNotDeclared`。这是静态校验，防止扩展越权
//! 注册未声明的能力。
//!
//! **Host API 兼容校验**（B5）：[`HOST_API_VERSION`] 即本 crate 版本；扩展
//! manifest 的 `version` 主版本号必须与 host 主版本一致（semver `^MAJOR` 匹配），
//! 否则首个 `register_*` 报 `InvalidManifest`。manifest 无独立 `api_version` 字段，
//! 以实际存在的 `version` 字段作为兼容性载体——进程内 Bundled 扩展与本 crate
//! 同 workspace 编译，主版本对齐即 API 契约对齐。
//!
//! **permissions 白名单校验**（B6）：manifest 声明的 `permissions` 必须 ⊆ 注册时
//! 配置的允许集（默认 [`DEFAULT_ALLOWED_PERMISSIONS`] 只读保守集），越界在首个
//! `register_*` 报 `PermissionDenied`。校验挂在注册流程内：任一失败即 init 失败、
//! bundle 整体丢弃。

use minicoding_core::extension::{
    Capability, ExtensionManifest, KeyBinding, Registrar, SlashCommand, StatusItem,
};
use minicoding_core::hooks::Hook;
use minicoding_core::model::ExtensionError;
use minicoding_core::prompt::PromptContributor;
use minicoding_core::tool::Tool;
use std::collections::HashSet;
use std::sync::Arc;

/// Host 扩展 API 版本（= 本 crate 语义化版本，B5 兼容校验基准）。
pub const HOST_API_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Host API 主版本号（兼容性判断只看主版本，见模块文档 B5）。
fn host_api_major() -> u64 {
    // 运行期解析即可：调用点仅在 registrar 构造与测试，无需 const fn
    // （`u64::from` 非 const trait，const 化需 nightly feature）。
    HOST_API_VERSION
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}

/// 默认权限白名单（B6）：只读工具根，保守最小集。
///
/// 参数级授权（如 `"shell.run:git *"`）不在默认集内——需要更宽权限的宿主经
/// [`BundleRegistrar::with_allowed_permissions`] 显式扩权（精确字符串匹配）。
pub const DEFAULT_ALLOWED_PERMISSIONS: &[&str] = &["fs.read", "fs.list", "fs.glob", "fs.grep"];

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
///
/// 构造时静态计算 B5（host API 主版本兼容）与 B6（permissions ⊆ 白名单）两道
/// 校验结果；任一不通过则在**首个** `register_*` 调用报错——init 即失败，bundle
/// 整体丢弃（事务式语义保持）。
pub struct BundleRegistrar {
    capabilities: Vec<Capability>,
    bundle: RegistrationBundle,
    /// B5：host API 不兼容原因（`None` = 兼容）。
    incompatible_reason: Option<String>,
    /// B6：白名单外的 permissions（空 = 全部合法）。
    undeclared_permissions: Vec<String>,
}

impl BundleRegistrar {
    /// 创建 registrar，传入 manifest 声明的 capabilities 用于校验。
    ///
    /// permissions 白名单取 [`DEFAULT_ALLOWED_PERMISSIONS`]（只读保守集）；
    /// 需要更宽权限的宿主用 [`BundleRegistrar::with_allowed_permissions`] 覆盖。
    #[must_use]
    pub fn new(manifest: &ExtensionManifest) -> Self {
        Self::with_allowed_permissions(
            manifest,
            DEFAULT_ALLOWED_PERMISSIONS.iter().map(|s| (*s).to_string()),
        )
    }

    /// 创建 registrar 并指定 permissions 白名单（B6 builder）。
    ///
    /// manifest 中任何不在集合内的 permission 会使首个 `register_*` 报
    /// [`ExtensionError::PermissionDenied`]。匹配为精确字符串比较——参数级授权
    /// （如 `"shell.run:git *"`）需按原文列入白名单。
    #[must_use]
    pub fn with_allowed_permissions<I>(manifest: &ExtensionManifest, allowed_permissions: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        let allowed_permissions: HashSet<String> = allowed_permissions.into_iter().collect();
        // B5：semver `^MAJOR` 兼容匹配（manifest 无独立 api_version 字段，
        // 以 version 字段为兼容载体，见模块文档）。
        let host_req = semver::VersionReq::parse(&format!("^{}", host_api_major())).ok();
        let incompatible_reason = match host_req {
            Some(req) if req.matches(&manifest.version) => None,
            Some(req) => Some(format!(
                "host api {} (req {req}) incompatible with extension version {}",
                HOST_API_VERSION, manifest.version
            )),
            None => Some(format!(
                "invalid host api version literal: {HOST_API_VERSION}"
            )),
        };

        // B6：diff 出白名单外的权限（保序去重，错误信息可读）。
        let mut undeclared_permissions: Vec<String> = manifest
            .permissions
            .iter()
            .filter(|p| !allowed_permissions.contains(*p))
            .cloned()
            .collect();
        undeclared_permissions.dedup();

        Self {
            capabilities: manifest.capabilities.clone(),
            bundle: RegistrationBundle::new(),
            incompatible_reason,
            undeclared_permissions,
        }
    }

    /// 提取收集到的注册项 bundle。
    #[must_use]
    pub fn into_bundle(self) -> RegistrationBundle {
        self.bundle
    }

    /// 检查 manifest 是否声明了指定 capability。
    fn check(&self, cap: Capability) -> Result<(), ExtensionError> {
        self.ensure_loadable()?;
        if self.capabilities.contains(&cap) {
            Ok(())
        } else {
            Err(ExtensionError::CapabilityNotDeclared(cap.as_str().into()))
        }
    }

    /// B5+B6 静态校验闸门：不通过时首个 `register_*` 即失败。
    fn ensure_loadable(&self) -> Result<(), ExtensionError> {
        if let Some(reason) = &self.incompatible_reason {
            return Err(ExtensionError::InvalidManifest(reason.clone()));
        }
        if !self.undeclared_permissions.is_empty() {
            return Err(ExtensionError::PermissionDenied(
                self.undeclared_permissions.join(", "),
            ));
        }
        Ok(())
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

    /// 与 host 主版本兼容的 manifest（B5 通过基线）。
    fn manifest_with_caps(caps: Vec<Capability>) -> ExtensionManifest {
        ExtensionManifest {
            id: ExtensionId("test".into()),
            version: Version::new(host_api_major(), 0, 0),
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

    #[test]
    fn host_api_version_is_crate_version() {
        // HOST_API_VERSION 必须与 CARGO_PKG_VERSION 一致（env! 直读，回归保护）。
        assert_eq!(HOST_API_VERSION, env!("CARGO_PKG_VERSION"));
    }

    // === B5：host API 主版本兼容校验 ===

    #[test]
    fn b5_same_major_passes() {
        let mut m = manifest_with_caps(vec![Capability::Tool]);
        // 同主版本不同 minor/patch：兼容。
        m.version = Version::new(host_api_major(), 9, 3);
        let mut r = BundleRegistrar::new(&m);
        r.register_tool(Arc::new(StubTool))
            .expect("同主版本应注册成功");
    }

    #[test]
    fn b5_different_major_rejected_at_register() {
        let mut m = manifest_with_caps(vec![Capability::Tool]);
        m.version = Version::new(host_api_major() + 1, 0, 0);
        let mut r = BundleRegistrar::new(&m);
        let err = r
            .register_tool(Arc::new(StubTool))
            .expect_err("跨主版本必须拒绝");
        assert!(matches!(err, ExtensionError::InvalidManifest(_)), "{err}");
        // bundle 未被污染（事务式：失败后 into_bundle 为空）。
        assert!(r.into_bundle().is_empty());
    }

    // === B6：permissions 白名单静态校验 ===

    #[test]
    fn b6_permissions_within_default_whitelist_pass() {
        let mut m = manifest_with_caps(vec![Capability::Tool]);
        m.permissions = vec!["fs.read".into(), "fs.grep".into()];
        let mut r = BundleRegistrar::new(&m);
        r.register_tool(Arc::new(StubTool))
            .expect("白名单内权限应注册成功");
    }

    #[test]
    fn b6_permissions_outside_whitelist_denied() {
        let mut m = manifest_with_caps(vec![Capability::Tool]);
        m.permissions = vec!["fs.read".into(), "shell.run".into()];
        let mut r = BundleRegistrar::new(&m);
        let err = r
            .register_tool(Arc::new(StubTool))
            .expect_err("白名单外权限必须拒绝");
        match err {
            ExtensionError::PermissionDenied(msg) => {
                assert!(msg.contains("shell.run"), "错误应指明越界权限: {msg}");
                assert!(!msg.contains("fs.read"), "白名单内权限不应列入错误: {msg}");
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
        assert!(r.into_bundle().is_empty());
    }

    #[test]
    fn b6_custom_whitelist_expands_permissions() {
        let mut m = manifest_with_caps(vec![Capability::Tool]);
        m.permissions = vec!["shell.run:git *".into()];
        let allowed: HashSet<String> = ["shell.run:git *".to_string()].into_iter().collect();
        let mut r = BundleRegistrar::with_allowed_permissions(&m, allowed);
        r.register_tool(Arc::new(StubTool))
            .expect("自定义白名单应放行参数级授权");
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
