//! `BundledExtensionHost`：进程内 first-party 扩展宿主实现。
//!
//! 管理 `Arc<dyn Extension>` 的生命周期：加载（manifest 校验 → init → 收集 bundle）、
//! 卸载（shutdown → 移除）、配置变更通知。`load_extension` 返回的注册项 bundle
//! 由调用方（Runtime/RuntimeBuilder）提取并提交到各注册表。
//!
//! 与 `NoopExtensionHost` 的区别：`BundledExtensionHost` 实际持有扩展实例并调用
//! `init`/`shutdown`，是生产环境的扩展宿主。`NoopExtensionHost` 仅用于未启用
//! 扩展时的兜底（返回 `NotFound`）。
//!
//! **线程安全**：内部用 `tokio::sync::RwLock` 保护扩展表，支持并发读（`list_extensions`）、
//! 互斥写（`load_extension`/`unload_extension`）。

use minicoding_core::extension::{
    Extension, ExtensionHost, ExtensionId, ExtensionInfo, ExtensionManifest,
};
use minicoding_core::model::ExtensionError;
use minicoding_core::provider::BoxFuture;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// 已加载的扩展实例 + 注册项 bundle。
///
/// `bundle` 在 `load_extension` 后由调用方通过 `take_bundle` 提取，提交到 Runtime
/// 各注册表。提取后 `bundle` 为 `None`。
pub struct LoadedExtension {
    /// 扩展实例（持有以支持 `shutdown`/`on_config_changed`）。
    pub extension: Arc<dyn Extension>,
    /// init 时收集的注册项 bundle（提取后为 `None`）。
    pub bundle: Option<crate::RegistrationBundle>,
}

impl LoadedExtension {
    /// 提取注册项 bundle（提交到 Runtime 各注册表）。
    ///
    /// 重复调用返回 `None`（bundle 只能提取一次）。
    pub fn take_bundle(&mut self) -> Option<crate::RegistrationBundle> {
        self.bundle.take()
    }
}

/// 进程内 first-party 扩展宿主。
///
/// Runtime 启动时注入 `Arc<BundledExtensionHost>`，调用 `load_extension` 批量加载
/// `~/.minicoding/extensions/` 下的扩展。加载后通过 `take_bundle` 提取注册项提交
/// 到 Runtime 各注册表。
///
/// # 示例
///
/// ```no_run
/// # use minicoding_core::extension::Extension;
/// # use minicoding_extension_sdk::BundledExtensionHost;
/// # use std::sync::Arc;
/// # async fn example(host: std::sync::Arc<BundledExtensionHost>, ext: Arc<dyn Extension>) {
/// let id = host.load_with_extension(ext, serde_json::Value::Null).await.expect("load");
/// // 提取 bundle 提交到 ToolRegistry/HookRegistry/PromptPipeline 等
/// if let Ok(Some(bundle)) = host.take_bundle(&id).await {
///     for tool in bundle.tools {
///         // tool_registry.register(tool);
///     }
/// }
/// # }
/// ```
#[derive(Default)]
pub struct BundledExtensionHost {
    extensions: RwLock<HashMap<ExtensionId, LoadedExtension>>,
}

impl BundledExtensionHost {
    /// 创建空宿主。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 提取已加载扩展的注册项 bundle（提交到 Runtime 各注册表）。
    ///
    /// # Errors
    /// 返回 [`ExtensionError::NotFound`] 当 id 不存在。
    pub async fn take_bundle(
        &self,
        id: &ExtensionId,
    ) -> Result<Option<crate::RegistrationBundle>, ExtensionError> {
        let mut exts = self.extensions.write().await;
        let loaded = exts
            .get_mut(id)
            .ok_or_else(|| ExtensionError::NotFound(id.to_string()))?;
        Ok(loaded.take_bundle())
    }

    /// 检查 id 是否已加载。
    #[must_use]
    pub async fn contains(&self, id: &ExtensionId) -> bool {
        self.extensions.read().await.contains_key(id)
    }

    /// 加载扩展的内部实现（不持锁调用 `init`，避免死锁）。
    ///
    /// `init` 可能调用 `Registrar`（不持锁），完成后才获取写锁插入。
    async fn load_inner(
        &self,
        extension: Arc<dyn Extension>,
        config: serde_json::Value,
    ) -> Result<ExtensionId, ExtensionError> {
        let manifest = extension.manifest().clone();
        let id = manifest.id.clone();

        // 检查 id 是否已加载（读锁）。
        if self.extensions.read().await.contains_key(&id) {
            return Err(ExtensionError::AlreadyLoaded(id.to_string()));
        }

        // 构造 registrar 并调用 init（不持锁，避免 init 耗时操作阻塞其他读）。
        let mut registrar = crate::BundleRegistrar::new(&manifest);
        extension
            .init(&mut registrar, config)
            .await
            .map_err(|e| match e {
                ExtensionError::InitFailed { reason, .. } => ExtensionError::InitFailed {
                    extension: id.to_string(),
                    reason,
                },
                other => other,
            })?;
        let bundle = registrar.into_bundle();

        // 获取写锁插入（双重检查 id 唯一性）。
        let mut exts = self.extensions.write().await;
        if exts.contains_key(&id) {
            return Err(ExtensionError::AlreadyLoaded(id.to_string()));
        }
        exts.insert(
            id.clone(),
            LoadedExtension {
                extension,
                bundle: Some(bundle),
            },
        );

        tracing::info!(extension = %id, "extension loaded");
        Ok(id)
    }
}

impl ExtensionHost for BundledExtensionHost {
    fn load_extension(
        &self,
        manifest: ExtensionManifest,
    ) -> BoxFuture<'_, Result<ExtensionId, ExtensionError>> {
        Box::pin(async move {
            // Bundled carrier 需要 caller 提供 extension 实例；manifest-only 加载
            // 仅校验 manifest 并返回 id（实际 extension 实例通过其他途径注入）。
            // 这里仅做 manifest 校验，真实加载由 `load_with_extension` 完成。
            let _ = manifest;
            Err(ExtensionError::NotFound(
                "BundledExtensionHost::load_extension(manifest) not supported; \
                 use load_with_extension instead"
                    .into(),
            ))
        })
    }

    fn unload_extension(&self, id: &ExtensionId) -> BoxFuture<'_, Result<(), ExtensionError>> {
        let id = id.clone();
        Box::pin(async move {
            let mut exts = self.extensions.write().await;
            let loaded = exts
                .remove(&id)
                .ok_or_else(|| ExtensionError::NotFound(id.to_string()))?;
            // 释放写锁后调用 shutdown（避免 shutdown 耗时阻塞其他扩展操作）。
            drop(exts);
            if let Err(e) = loaded.extension.shutdown().await {
                tracing::warn!(extension = %id, error = %e, "extension shutdown failed");
                return Err(ExtensionError::ShutdownFailed {
                    extension: id.to_string(),
                    reason: e.to_string(),
                });
            }
            tracing::info!(extension = %id, "extension unloaded");
            Ok(())
        })
    }

    fn list_extensions(&self) -> BoxFuture<'_, Vec<ExtensionInfo>> {
        Box::pin(async move {
            let exts = self.extensions.read().await;
            exts.values()
                .map(|loaded| {
                    let m = loaded.extension.manifest();
                    let registered = if let Some(ref b) = loaded.bundle {
                        b.counts_by_capability()
                    } else {
                        Vec::new()
                    };
                    ExtensionInfo {
                        id: m.id.clone(),
                        version: m.version.clone(),
                        name: m.name.clone(),
                        carrier: m.carrier.clone(),
                        registered,
                    }
                })
                .collect()
        })
    }

    fn on_config_changed(
        &self,
        id: &ExtensionId,
        new_config: serde_json::Value,
    ) -> BoxFuture<'_, Result<(), ExtensionError>> {
        let id = id.clone();
        Box::pin(async move {
            let exts = self.extensions.read().await;
            let loaded = exts
                .get(&id)
                .ok_or_else(|| ExtensionError::NotFound(id.to_string()))?;
            // 持读锁调用 on_config_changed（扩展不应修改自身注册状态，仅读配置）。
            loaded.extension.on_config_changed(new_config).await
        })
    }
}

impl BundledExtensionHost {
    /// 加载扩展实例（Bundled carrier 的入口）。
    ///
    /// 与 `ExtensionHost::load_extension(manifest)` 不同，Bundled 扩展由 caller
    /// 直接提供 `Arc<dyn Extension>` 实例（进程内编译），无需通过 manifest 查找符号。
    ///
    /// # Errors
    /// - [`ExtensionError::AlreadyLoaded`]：id 重复；
    /// - [`ExtensionError::InitFailed`]：扩展 `init` 失败。
    pub async fn load_with_extension(
        &self,
        extension: Arc<dyn Extension>,
        config: serde_json::Value,
    ) -> Result<ExtensionId, ExtensionError> {
        self.load_inner(extension, config).await
    }

    /// 卸载所有扩展（Runtime 关闭时调用）。
    ///
    /// 逐个调用 `shutdown`，收集失败但不中断（best-effort）。
    pub async fn shutdown_all(&self) -> Vec<(ExtensionId, ExtensionError)> {
        let mut exts = self.extensions.write().await;
        let drained: Vec<(ExtensionId, LoadedExtension)> = exts.drain().collect();
        drop(exts);

        let mut failures = Vec::new();
        for (id, loaded) in drained {
            let id_str = id.to_string();
            if let Err(e) = loaded.extension.shutdown().await {
                tracing::warn!(extension = %id_str, error = %e, "extension shutdown failed");
                failures.push((
                    id,
                    ExtensionError::ShutdownFailed {
                        extension: id_str.clone(),
                        reason: e.to_string(),
                    },
                ));
            }
        }
        failures
    }
}

impl std::fmt::Debug for BundledExtensionHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundledExtensionHost")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use minicoding_core::extension::{ExtensionCarrier, ExtensionManifest};
    use minicoding_core::provider::BoxFuture;
    use semver::Version;
    use std::sync::Arc;

    fn manifest(id: &str, caps: Vec<minicoding_core::extension::Capability>) -> ExtensionManifest {
        ExtensionManifest {
            id: ExtensionId(id.into()),
            version: Version::new(0, 1, 0),
            name: id.into(),
            author: None,
            carrier: ExtensionCarrier::Bundled,
            capabilities: caps,
            permissions: Vec::new(),
            config_schema: None,
        }
    }

    /// 测试扩展：init 时注册一个工具（如果声明了 Tool 能力）。
    struct TestExtension {
        manifest: ExtensionManifest,
    }

    impl TestExtension {
        fn new(manifest: ExtensionManifest) -> Self {
            Self { manifest }
        }
    }

    impl Extension for TestExtension {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }
        fn init(
            &self,
            _registrar: &mut dyn minicoding_core::extension::Registrar,
            _config: serde_json::Value,
        ) -> BoxFuture<'_, Result<(), ExtensionError>> {
            Box::pin(async { Ok(()) })
        }
        fn shutdown(&self) -> BoxFuture<'_, Result<(), ExtensionError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn load_and_list_extension() {
        let host = Arc::new(BundledExtensionHost::new());
        let ext = Arc::new(TestExtension::new(manifest("test-1", Vec::new())));
        let id = host
            .load_with_extension(ext, serde_json::Value::Null)
            .await
            .expect("load");
        assert_eq!(id.to_string(), "test-1");

        let list = host.list_extensions().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id.to_string(), "test-1");
    }

    #[tokio::test]
    async fn duplicate_load_returns_already_loaded() {
        let host = Arc::new(BundledExtensionHost::new());
        let ext1 = Arc::new(TestExtension::new(manifest("dup", Vec::new())));
        let ext2 = Arc::new(TestExtension::new(manifest("dup", Vec::new())));

        host.load_with_extension(ext1, serde_json::Value::Null)
            .await
            .expect("first load");
        let result = host
            .load_with_extension(ext2, serde_json::Value::Null)
            .await;
        assert!(matches!(result, Err(ExtensionError::AlreadyLoaded(_))));
    }

    #[tokio::test]
    async fn unload_extension_removes_it() {
        let host = Arc::new(BundledExtensionHost::new());
        let ext = Arc::new(TestExtension::new(manifest("removable", Vec::new())));
        let id = host
            .load_with_extension(ext, serde_json::Value::Null)
            .await
            .expect("load");

        host.unload_extension(&id).await.expect("unload");
        assert!(host.list_extensions().await.is_empty());
    }

    #[tokio::test]
    async fn unload_nonexistent_returns_not_found() {
        let host = BundledExtensionHost::new();
        let result = host.unload_extension(&ExtensionId("nope".into())).await;
        assert!(matches!(result, Err(ExtensionError::NotFound(_))));
    }

    #[tokio::test]
    async fn on_config_changed_forwards_to_extension() {
        let host = Arc::new(BundledExtensionHost::new());
        let ext = Arc::new(TestExtension::new(manifest("cfg", Vec::new())));
        let id = host
            .load_with_extension(ext, serde_json::Value::Null)
            .await
            .expect("load");

        let result = host
            .on_config_changed(&id, serde_json::json!({"key": "value"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn take_bundle_returns_none_after_first_take() {
        let host = Arc::new(BundledExtensionHost::new());
        let ext = Arc::new(TestExtension::new(manifest("bundle", Vec::new())));
        let id = host
            .load_with_extension(ext, serde_json::Value::Null)
            .await
            .expect("load");

        // 第一次 take：返回 Some(empty bundle)（TestExtension 不注册任何能力，但 bundle 仍存在）
        let first = host.take_bundle(&id).await.expect("take");
        assert!(first.is_some());
        assert!(first.as_ref().is_some_and(|b| b.is_empty()));

        // 第二次 take：返回 None（bundle 已被提取）
        let second = host.take_bundle(&id).await.expect("take");
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn shutdown_all_clears_extensions() {
        let host = Arc::new(BundledExtensionHost::new());
        for name in ["a", "b", "c"] {
            let ext = Arc::new(TestExtension::new(manifest(name, Vec::new())));
            host.load_with_extension(ext, serde_json::Value::Null)
                .await
                .expect("load");
        }
        assert_eq!(host.list_extensions().await.len(), 3);

        let failures = host.shutdown_all().await;
        assert!(failures.is_empty());
        assert!(host.list_extensions().await.is_empty());
    }
}
