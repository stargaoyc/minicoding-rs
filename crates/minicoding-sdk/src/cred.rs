//! 凭证读取（A11 自 cli/cred.rs 迁移 `load_api_key`；store/delete CLI UX 留在 cli）。
//!
//! 优先级：OS keyring → 文件 fallback `~/.minicoding/credentials`(0600)。
//! env/CLI 参数由 builder 直接处理，不经本模块。详见 `security.md` §6。

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use minicoding_core::paths;
// ARCH-4（2026-08-26 R3 审查）：keyring service/account 常量下沉 core
// （`minicoding_core::util`）——四处私有复制曾靠注释维持一致性，一处改名即
// 静默 split-brain。KEYRING_SERVICE/KEYRING_ACCOUNT 经下方 use 引入。
#[cfg(feature = "cred-keyring")]
use minicoding_core::util::{KEYRING_ACCOUNT, KEYRING_SERVICE};
use std::fs;

/// 从 keyring 或文件 fallback 加载 API key。
///
/// 优先尝试 OS keyring；失败时降级到 `~/.minicoding/credentials` 文件。
/// 两者均不可用且无 env 兜底时返回 `None`，由调用方决定是否报错。
///
/// # Errors
/// 仅在文件 fallback 存在但读取失败（IO 错误、权限错误）时返回错误；
/// keyring 不可用或文件不存在均返回 `Ok(None)`。
pub fn load_api_key() -> Result<Option<String>> {
    // 1. 尝试 OS keyring
    match try_keyring_get() {
        Ok(Some(key)) => {
            tracing::debug!("api key 从 OS keyring 加载");
            return Ok(Some(key));
        }
        Ok(None) => {
            tracing::debug!("OS keyring 中无 minicoding 凭证");
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "OS keyring 不可用，降级到文件 fallback（C-04）"
            );
        }
    }

    // 2. 文件 fallback
    load_api_key_from_file()
}

/// 仅从文件 fallback 加载 API key（不查 keyring，测试用）。
///
/// # Errors
/// 文件存在但读取失败时返回错误；文件不存在返回 `Ok(None)`。
pub fn load_api_key_from_file() -> Result<Option<String>> {
    let path = credentials_file_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("读取 credentials 文件失败: {path}"))?;
    let key = raw.trim().to_string();
    if key.is_empty() {
        return Ok(None);
    }
    tracing::debug!("api key 从文件 fallback 加载");
    Ok(Some(key))
}

/// 从 OS keyring 读取 API key。
///
/// 返回 `Ok(None)` 表示 keyring 可用但无对应 entry；
/// 返回 `Err` 表示 keyring 不可用（应降级到文件 fallback）。
#[cfg(feature = "cred-keyring")]
fn try_keyring_get() -> Result<Option<String>> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("创建 keyring entry 失败")?;
    match entry.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("keyring get 失败: {e}")),
    }
}

/// 文件 fallback 路径：`~/.minicoding/credentials`（0600 权限）。
fn credentials_file_path() -> Result<Utf8PathBuf> {
    Ok(paths::minicoding_home()?.join("credentials"))
}

/// `cred-keyring` feature 关闭时的桩：直接报告不可用，调用方落到文件 fallback。
#[cfg(not(feature = "cred-keyring"))]
fn try_keyring_get() -> Result<Option<String>> {
    Ok(None)
}
