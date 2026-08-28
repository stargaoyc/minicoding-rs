//! 桌面端配置管理：读写 `~/.minicoding/config.toml` + OS keyring（C-04）。
//!
//! 安装包用户无 CLI，通过 Tauri invoke 命令读写配置：
//! - 非敏感配置（`api_base`/`model`/`provider`/`name`）→ `config.toml`
//! - API key → OS keyring（与 CLI `cred.rs` 共享 `KEYRING_SERVICE`，C-04 凭证不落明文）
//!
//! sidecar 启动时由 [`crate::sidecar`] 读取此模块的配置 + keyring，通过 CLI 参数
//! 传给 `minicoding-server`（内存传递，不下传 env，C-04）。
//!
//! 详见 `docs/design.md` §26.5。

use anyhow::{Context, Result};
use minicoding_core::config::{ProviderConfig, load_config};
use minicoding_core::paths;

// ARCH-4（2026-08-26 R3 审查）：keyring service/account 常量下沉 core 单一
// 事实来源（desktop/CLI/server/sdk 四端读写同一 entry）。
#[cfg(feature = "desktop")]
use minicoding_core::util::{KEYRING_ACCOUNT, KEYRING_SERVICE};

/// 读取 provider 配置（从 `~/.minicoding/config.toml`）。
///
/// 配置文件不存在时返回默认值（`openai` + `https://api.openai.com/v1`）。
///
/// # Errors
/// 配置文件存在但解析失败且无 last-known-good 回退时返回错误。
pub fn get_provider_config() -> Result<ProviderConfig> {
    let config = load_config().map_err(|e| anyhow::anyhow!("加载配置失败: {e}"))?;
    Ok(config.provider)
}

/// 保存 provider 配置到 `~/.minicoding/config.toml`。
///
/// 读取现有完整配置（保留 `context`/`tools`/`hooks` 等其他段），替换 `[provider]` 段，
/// 原子写入（tmp + rename，避免崩溃导致配置文件损坏）。
///
/// **不写入 `api_key` 明文**：`ProviderConfig.api_key` 字段留空或用 `env:VAR` 语法，
/// 真实凭证由 [`store_api_key`] 写入 OS keyring（C-04）。
///
/// **M-10 防陈旧写**：`expected_revision` 为 `Some(x)` 时，若当前配置 `revision != x`
/// （其他客户端已抢先保存）拒绝写入并返回 `StaleWrite` 错误文本，不覆盖；保存成功后
/// `revision` 原子自增。`None` 表示无条件写（兼容旧调用方）。
///
/// # Errors
/// 配置文件序列化失败、IO 错误、revision 不匹配时返回错误。
pub fn save_provider_config(
    provider: ProviderConfig,
    expected_revision: Option<u64>,
) -> Result<()> {
    // 读取现有配置（保留其他段）
    let mut config = load_config()
        .map_err(|e| anyhow::anyhow!("加载配置失败: {e}"))
        .unwrap_or_default();
    if let Some(expected) = expected_revision
        && config.revision != expected
    {
        return Err(anyhow::anyhow!(
            "StaleWrite: 配置修订号不匹配（当前 {}，期望 {}），请刷新后重试",
            config.revision,
            expected
        ));
    }
    config.revision = config.revision.saturating_add(1);
    // ARCH-R6-2（2026-08-28 R6 审查）：前端传入即剥离 api_key——doc comment 声称
    // "不写入 api_key 明文"但此前直接 `config.provider = provider`，前端若传
    // key 则明文落 config.toml（C-04 相悖；http.rs 侧已有剥离，desktop 无）。
    // 真实凭证经 `store_api_key` 写 OS keyring。
    let mut provider = provider;
    provider.api_key.clear();
    config.provider = provider;

    let config_path = paths::config_path().context("无法确定配置文件路径")?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("创建配置目录失败: {parent}"))?;
    }
    let serialized = toml::to_string_pretty(&config).context("序列化配置失败")?;
    // 原子写入：tmp + rename
    let tmp = config_path.with_extension("toml.tmp");
    std::fs::write(&tmp, serialized.as_bytes())
        .with_context(|| format!("写入配置临时文件失败: {tmp}"))?;
    std::fs::rename(&tmp, &config_path)
        .with_context(|| format!("rename 配置文件失败: {tmp} -> {config_path}"))?;
    log::info!("provider 配置已保存: {config_path}");
    Ok(())
}

/// 从 OS keyring 加载 API key（与 CLI `cred.rs` 共享 entry）。
///
/// FE-5（2026-08-26 R3 审查）：keyring 不可用时降级到
/// `~/.minicoding/credentials` 文件 fallback（0600，与 CLI/sdk 同路径同格式）
/// ——headless Linux（无 secret-service 守护）用户此前直接进入错误屏，
/// 而 CLI 可用；两形态行为对齐。
///
/// 返回 `Ok(None)` 表示 keyring 与文件均无 key；
/// 返回 `Err` 仅当文件存在但读取失败。
///
/// # Errors
/// 文件 fallback 存在但读取失败时返回错误。
#[cfg(feature = "desktop")]
pub fn load_api_key() -> Result<Option<String>> {
    // 1. 尝试 OS keyring
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("创建 keyring entry 失败")?;
    match entry.get_password() {
        Ok(key) => return Ok(Some(key)),
        Err(keyring::Error::NoEntry) => {
            log::info!("OS keyring 中无 minicoding 凭证，尝试文件 fallback");
        }
        Err(e) => {
            log::warn!("OS keyring 不可用（{e}），降级到文件 fallback（C-04）");
        }
    }

    // 2. 文件 fallback：`~/.minicoding/credentials`（与 sdk/cred.rs 同一路径约定；
    //    ARCH-4 常量集中化后可改经共享实现）
    let home = minicoding_core::paths::minicoding_home().context("定位 MINICODING_HOME 失败")?;
    let path = home.join("credentials");
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读取 credentials 文件失败: {path}"))?;
    let key = raw.trim().to_string();
    if key.is_empty() {
        return Ok(None);
    }
    log::info!("api key 从文件 fallback 加载");
    Ok(Some(key))
}

/// 写入 API key 到 OS keyring。
///
/// 与 CLI `minicoding cred store` 写入同一 entry，两边共享凭证。
///
/// # Errors
/// keyring 写入失败时返回错误。
#[cfg(feature = "desktop")]
pub fn store_api_key(key: &str) -> Result<()> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("创建 keyring entry 失败")?;
    entry
        .set_password(key)
        .map_err(|e| anyhow::anyhow!("keyring set 失败: {e}"))?;
    log::info!("api key 已写入 OS keyring");
    Ok(())
}

/// 删除 keyring 中的 API key。
///
/// # Errors
/// keyring 删除失败（非 NoEntry）时返回错误。
#[cfg(feature = "desktop")]
pub fn delete_api_key() -> Result<()> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("创建 keyring entry 失败")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("keyring delete 失败: {e}")),
    }
}

/// 返回配置文件路径（供前端"打开配置文件"功能用）。
///
/// # Errors
/// 无法确定 minicoding home 目录时返回错误。
pub fn config_file_path() -> Result<camino::Utf8PathBuf> {
    paths::config_path().context("无法确定配置文件路径")
}

/// 读取上下文配置（`[context]` 段：turn 超时 / 压缩开关）。
///
/// 配置文件不存在时返回默认值（`ContextConfig::default()`，turn 超时 600s、
/// 压缩开启）。
///
/// # Errors
/// 配置文件存在但解析失败且无 last-known-good 回退时返回错误。
pub fn get_context_config() -> Result<minicoding_core::config::ContextConfig> {
    let config = load_config().map_err(|e| anyhow::anyhow!("加载配置失败: {e}"))?;
    Ok(config.context)
}

/// 保存上下文配置（`[context]` 段）到 `~/.minicoding/config.toml`。
///
/// 读取现有完整配置（保留 `provider`/`tools`/`hooks` 等其他段），替换 `[context]` 段，
/// 原子写入（tmp + rename）。sidecar 启动时 `minicoding serve` 会读取本段生效。
///
/// # Errors
/// 配置文件序列化失败、IO 错误时返回错误。
pub fn save_context_config(context: minicoding_core::config::ContextConfig) -> Result<()> {
    let mut config = load_config()
        .map_err(|e| anyhow::anyhow!("加载配置失败: {e}"))
        .unwrap_or_default();
    config.context = context;

    let config_path = paths::config_path().context("无法确定配置文件路径")?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("创建配置目录失败: {parent}"))?;
    }
    let serialized = toml::to_string_pretty(&config).context("序列化配置失败")?;
    let tmp = config_path.with_extension("toml.tmp");
    std::fs::write(&tmp, serialized.as_bytes())
        .with_context(|| format!("写入配置临时文件失败: {tmp}"))?;
    std::fs::rename(&tmp, &config_path)
        .with_context(|| format!("rename 配置文件失败: {tmp} -> {config_path}"))?;
    log::info!("context 配置已保存: {config_path}");
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    #![allow(unsafe_code)] // Rust 2024: set_var/remove_var 标记 unsafe
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// `save_provider_config` + `get_provider_config` 往返测试（隔离 MINICODING_HOME）。
    #[test]
    fn provider_config_round_trip() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let tmp = tempfile::tempdir().expect("create tempdir");
        // SAFETY: 持有 ENV_LOCK 保证串行，无并发 set_var 风险。
        unsafe {
            std::env::set_var("MINICODING_HOME", tmp.path());
        }

        let provider = ProviderConfig {
            default: "openai".into(),
            name: Some("deepseek".into()),
            api_base: "https://api.deepseek.com/v1".into(),
            api_key: String::new(), // 不落明文
            model: "deepseek-chat".into(),
            timeout_sec: 60,
            max_retries: 3,
            small: None,
        };

        save_provider_config(provider.clone(), None).expect("save provider config");

        let loaded = get_provider_config().expect("get provider config");
        assert_eq!(loaded.default, "openai");
        assert_eq!(loaded.name.as_deref(), Some("deepseek"));
        assert_eq!(loaded.api_base, "https://api.deepseek.com/v1");
        assert_eq!(loaded.model, "deepseek-chat");
        assert!(loaded.api_key.is_empty(), "api_key 不应落明文");

        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MINICODING_HOME");
        }
    }

    /// M-10：revision 自增 + 陈旧写被拒。
    #[test]
    fn stale_write_rejected_and_revision_increments() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let tmp = tempfile::tempdir().expect("create tempdir");
        // SAFETY: 持有 ENV_LOCK 保证串行，无并发 set_var 风险。
        unsafe {
            std::env::set_var("MINICODING_HOME", tmp.path());
        }

        let provider = ProviderConfig {
            default: "openai".into(),
            name: None,
            api_base: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: "gpt-4".into(),
            timeout_sec: 60,
            max_retries: 3,
            small: None,
        };
        // 首次保存 revision 0 → 1
        save_provider_config(provider.clone(), Some(0)).expect("首写应成功");
        assert_eq!(
            load_config().expect("load").revision,
            1,
            "保存后 revision 应自增"
        );

        // 用陈旧 revision 0 再写 → StaleWrite 拒绝
        let err = save_provider_config(provider.clone(), Some(0)).expect_err("陈旧写应被拒");
        assert!(
            err.to_string().contains("StaleWrite"),
            "错误应标记 StaleWrite: {err}"
        );
        assert_eq!(
            load_config().expect("load").revision,
            1,
            "拒绝后 revision 不变"
        );

        // 用当前 revision 1 写 → 成功，自增到 2
        save_provider_config(provider, Some(1)).expect("当前 revision 写应成功");
        assert_eq!(load_config().expect("load").revision, 2);

        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MINICODING_HOME");
        }
    }

    /// `get_provider_config` 在无配置文件时返回默认值。
    #[test]
    fn get_provider_config_defaults_when_no_file() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let tmp = tempfile::tempdir().expect("create tempdir");
        // SAFETY: 持有 ENV_LOCK 保证串行。
        unsafe {
            std::env::set_var("MINICODING_HOME", tmp.path());
        }

        let loaded = get_provider_config().expect("get provider config");
        assert_eq!(loaded.default, "openai");
        assert_eq!(loaded.api_base, "https://api.openai.com/v1");

        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MINICODING_HOME");
        }
    }
}
