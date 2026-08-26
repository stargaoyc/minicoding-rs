//! 凭证存储：OS keyring + 文件 fallback（T-M4-11，C-04）。
//!
//! API key 优先级（高 → 低）：
//! 1. CLI `--api-key` 参数（由 `builder` 直接传入，不经本模块）；
//! 2. `OPENAI_API_KEY` 环境变量（由 `builder` 读取，不经本模块）；
//! 3. OS keyring（本模块 `load_api_key`）；
//! 4. 文件 fallback `~/.minicoding/credentials`（0600 权限）。
//!
//! ## keyring 不可用时降级
//!
//! Linux 无 D-Bus / 无 secret-service 守护时 `keyring` crate 返回错误，本模块
//! 自动降级到文件 fallback（0600 权限），不阻塞启动。降级事件记 warn 日志。
//!
//! ## 凭证不出现在配置明文
//!
//! `config.toml` 中 `[provider] api_key` 字段仅支持 `env:VAR` 语法或留空；
//! 真实凭证由本模块管理，不落 `config.toml` 明文（C-04）。
//!
//! 详见 `docs/security.md` §6、`docs/rules.md` C-04。

use anyhow::{Context, Result};
// A11：实现迁 sdk，测试仍引用文件 helper
use camino::Utf8PathBuf;
use minicoding_core::paths;
pub use minicoding_sdk::cred::load_api_key_from_file;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

// ARCH-4（2026-08-26 R3 审查）：常量下沉 core 单一事实来源（四处私有复制
// 曾靠注释维持一致，改名即静默 split-brain）。
use minicoding_core::util::{KEYRING_ACCOUNT, KEYRING_SERVICE};

/// 文件 fallback 路径：`~/.minicoding/credentials`（0600 权限）。
fn credentials_file_path() -> Result<Utf8PathBuf> {
    Ok(paths::minicoding_home()?.join("credentials"))
}

/// 从 keyring 或文件 fallback 加载 API key。
///
/// 优先尝试 OS keyring；失败时降级到 `~/.minicoding/credentials` 文件。
/// 两者均不可用且无 env 兜底时返回 `None`，由调用方决定是否报错。
///
/// # Errors
/// 仅在文件 fallback 存在但读取失败（IO 错误、权限错误）时返回错误；
/// keyring 不可用或文件不存在均返回 `Ok(None)`。
pub fn load_api_key() -> Result<Option<String>> {
    minicoding_sdk::cred::load_api_key()
}

/// 把 API key 写入 keyring（失败时降级到文件 fallback）。
///
/// # Errors
/// keyring 写入失败且文件 fallback 写入失败时返回错误。
pub fn store_api_key(key: &str) -> Result<()> {
    // 1. 尝试 OS keyring
    match try_keyring_set(key) {
        Ok(()) => {
            tracing::info!("api key 已写入 OS keyring");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "OS keyring 写入失败，降级到文件 fallback（C-04）"
            );
        }
    }

    // 2. 文件 fallback
    store_api_key_to_file(key)
}

/// 仅写入文件 fallback（不查 keyring，测试用）。
///
/// # Errors
/// 文件 IO 失败时返回错误。
fn store_api_key_to_file(key: &str) -> Result<()> {
    let path = credentials_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 credentials 父目录失败: {parent}"))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, key.as_bytes())
        .with_context(|| format!("写入 credentials 临时文件失败: {tmp}"))?;
    // Unix 设置 0600 权限（Windows 文件 ACL 由用户目录权限保障）
    #[cfg(unix)]
    {
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置 credentials 临时文件权限失败: {tmp}"))?;
    }
    fs::rename(&tmp, &path).with_context(|| format!("rename credentials 失败: {tmp} -> {path}"))?;
    tracing::info!("api key 已写入文件 fallback（0600）");
    Ok(())
}

/// 写入 OS keyring。
fn try_keyring_set(key: &str) -> Result<()> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("创建 keyring entry 失败")?;
    entry
        .set_password(key)
        .map_err(|e| anyhow::anyhow!("keyring set 失败: {e}"))
}

/// 删除 keyring 中的 API key（`minicoding cred delete` 用）。
///
/// # Errors
/// keyring 删除失败时返回错误（文件 fallback 不存在删除概念，best-effort 删除文件）。
pub fn delete_api_key() -> Result<()> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("创建 keyring entry 失败")?;
    match entry.delete_credential() {
        Ok(()) => {
            tracing::info!("keyring 凭证已删除");
        }
        Err(keyring::Error::NoEntry) => {
            tracing::debug!("keyring 中无凭证，跳过删除");
        }
        Err(e) => {
            return Err(anyhow::anyhow!("keyring delete 失败: {e}"));
        }
    }
    // best-effort 删除文件 fallback
    let path = credentials_file_path()?;
    if path.exists() {
        let _ = fs::remove_file(&path);
        tracing::info!("文件 fallback 凭证已删除");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    #![allow(unsafe_code)] // 测试中 set_var/remove_var 在 Rust 2024 标记为 unsafe
    use super::*;
    use std::sync::Mutex;

    // 所有修改 MINICODING_HOME 的测试共享此锁，强制串行执行（env var 是进程全局）。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 文件 fallback 写入 + 读取 + 删除的集成测试。
    ///
    /// 使用临时 `MINICODING_HOME` 隔离，避免污染真实用户目录。
    /// keyring 部分依赖 OS 守护进程，CI 可能不可用，故仅测文件 fallback。
    #[test]
    fn file_fallback_round_trip() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let tmp = tempfile::tempdir().expect("create tempdir");
        // SAFETY: 持有 ENV_LOCK 保证串行，无并发 set_var 风险。
        unsafe {
            std::env::set_var("MINICODING_HOME", tmp.path());
        }

        // 写入（绕过 keyring，仅测文件 fallback）
        store_api_key_to_file("sk-test-key-12345").expect("store api key to file");

        // 读取
        let loaded = load_api_key_from_file().expect("load api key from file");
        assert_eq!(loaded.as_deref(), Some("sk-test-key-12345"));

        // 删除
        delete_api_key().expect("delete api key");
        let after = load_api_key_from_file().expect("load after delete");
        assert!(after.is_none(), "credentials should be deleted");

        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MINICODING_HOME");
        }
    }

    /// 空内容应被视为无凭证。
    #[test]
    fn empty_file_returns_none() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let tmp = tempfile::tempdir().expect("create tempdir");
        // SAFETY: 持有 ENV_LOCK 保证串行。
        unsafe {
            std::env::set_var("MINICODING_HOME", tmp.path());
        }

        let path = credentials_file_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "   \n\n").unwrap();

        let loaded = load_api_key_from_file().expect("load api key from file");
        assert!(loaded.is_none(), "空文件应返回 None");

        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MINICODING_HOME");
        }
    }

    /// 不存在文件应返回 None（不报错）。
    #[test]
    fn missing_file_returns_none() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let tmp = tempfile::tempdir().expect("create tempdir");
        // SAFETY: 持有 ENV_LOCK 保证串行。
        unsafe {
            std::env::set_var("MINICODING_HOME", tmp.path());
        }

        let loaded = load_api_key_from_file().expect("load api key from file");
        assert!(loaded.is_none(), "不存在的文件应返回 None");

        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MINICODING_HOME");
        }
    }

    /// 文件 fallback 权限应为 0600（仅 Linux/Unix 校验）。
    #[cfg(unix)]
    #[test]
    fn file_permissions_are_0600() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let tmp = tempfile::tempdir().expect("create tempdir");
        // SAFETY: 持有 ENV_LOCK 保证串行。
        unsafe {
            std::env::set_var("MINICODING_HOME", tmp.path());
        }

        store_api_key_to_file("sk-test-perms").expect("store api key to file");

        let path = credentials_file_path().unwrap();
        let meta = std::fs::metadata(&path).expect("metadata");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials 文件权限应为 0600");

        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("MINICODING_HOME");
        }
    }
}
