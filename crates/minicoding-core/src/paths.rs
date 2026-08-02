//! 路径约定（见 `data-model.md` §3.0）。
//!
//! `MINICODING_HOME` 环境变量覆盖根目录，默认 `~/.minicoding/`。

use camino::Utf8PathBuf;
use std::env;

/// 获取 `MINICODING_HOME` 根目录（默认 `~/.minicoding/`）。
///
/// # Errors
/// 当 home 目录无法确定时返回错误。
pub fn minicoding_home() -> Result<Utf8PathBuf, std::io::Error> {
    if let Ok(p) = env::var("MINICODING_HOME") {
        return Ok(Utf8PathBuf::from(p));
    }
    let home = home::home_dir().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "cannot determine home dir")
    })?;
    Ok(Utf8PathBuf::from_path_buf(home)
        .map_err(|p| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} not UTF-8", p.display()),
            )
        })?
        .join(".minicoding"))
}

/// 会话日志目录。
///
/// # Errors
/// 当 home 目录无法确定时返回错误。
pub fn sessions_dir() -> Result<Utf8PathBuf, std::io::Error> {
    Ok(minicoding_home()?.join("sessions"))
}

/// 配置文件路径。
///
/// # Errors
/// 当 home 目录无法确定时返回错误。
pub fn config_path() -> Result<Utf8PathBuf, std::io::Error> {
    Ok(minicoding_home()?.join("config.toml"))
}

/// last-known-good 配置回退路径（见 `design.md` §12）。
///
/// # Errors
/// 当 home 目录无法确定时返回错误。
pub fn last_known_good_path() -> Result<Utf8PathBuf, std::io::Error> {
    Ok(minicoding_home()?.join(".last-known-good.toml"))
}

/// 审计日志路径。
///
/// # Errors
/// 当 home 目录无法确定时返回错误。
pub fn audit_log_path() -> Result<Utf8PathBuf, std::io::Error> {
    Ok(minicoding_home()?.join("audit.log"))
}

/// 记忆目录。
///
/// # Errors
/// 当 home 目录无法确定时返回错误。
pub fn memory_dir() -> Result<Utf8PathBuf, std::io::Error> {
    Ok(minicoding_home()?.join("memory"))
}

/// MCP project 作用域批准库路径（`~/.minicoding/mcp_choices.toml`，0600 权限）。
///
/// 存储用户对 project 作用域 MCP server 的批准/拒绝决策（C-24）。
///
/// # Errors
/// 当 home 目录无法确定时返回错误。
pub fn mcp_choices_path() -> Result<Utf8PathBuf, std::io::Error> {
    Ok(minicoding_home()?.join("mcp_choices.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 序列化所有依赖 `MINICODING_HOME` 的测试，避免并行环境变量竞争。
    /// `unwrap_or_else(into_inner)` 从 poison 中恢复（前序测试 panic 不阻塞后续）。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 测试期间临时修改环境变量，`Drop` 时恢复原值，保证测试隔离。
    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        /// 设置环境变量为指定值，返回恢复用的 guard。
        fn set(key: &'static str, value: &str) -> Self {
            let original = env::var(key).ok();
            // SAFETY: 测试模块内通过 ENV_LOCK 串行化所有 MINICODING_HOME 访问，
            // 无并发 set/remove；测试运行期间无其他线程读取该变量。
            unsafe { env::set_var(key, value) };
            Self { key, original }
        }

        /// 删除环境变量，返回恢复用的 guard。
        fn remove(key: &'static str) -> Self {
            let original = env::var(key).ok();
            // SAFETY: 同 set()，ENV_LOCK 保证串行访问。
            unsafe { env::remove_var(key) };
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: 同 set()，ENV_LOCK 保证串行访问；Drop 在测试 scope 结束时
            // 同步调用，无并发。
            match &self.original {
                Some(v) => unsafe { env::set_var(self.key, v) },
                None => unsafe { env::remove_var(self.key) },
            }
        }
    }

    /// 获取环境变量锁，串行化所有 paths 测试。
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn minicoding_home_uses_env_var_when_set() {
        let _g = lock_env();
        let dir = tempfile::tempdir().expect("创建临时目录");
        let expected_str = dir.path().to_str().expect("tempdir 路径应为 UTF-8");
        let _guard = EnvGuard::set("MINICODING_HOME", expected_str);

        let home = minicoding_home().expect("获取 minicoding_home");
        assert_eq!(home, Utf8PathBuf::from(expected_str));
        // 返回类型已保证 UTF-8
        assert!(home.as_str().chars().all(|c| !c.is_control()));
    }

    #[test]
    fn minicoding_home_falls_back_to_default_when_unset() {
        let _g = lock_env();
        // 显式移除环境变量，模拟未设置
        let _guard = EnvGuard::remove("MINICODING_HOME");

        let home = minicoding_home().expect("获取 minicoding_home");
        // 默认 ~/.minicoding（依赖 home crate 解析用户目录）
        let user_home = home::home_dir().expect("home_dir 应可解析");
        let expected = Utf8PathBuf::from_path_buf(user_home)
            .expect("user home 应为 UTF-8")
            .join(".minicoding");
        assert_eq!(home, expected);
    }

    #[test]
    fn subdirs_are_relative_to_minicoding_home() {
        let _g = lock_env();
        let dir = tempfile::tempdir().expect("创建临时目录");
        let home_str = dir.path().to_str().expect("tempdir 路径应为 UTF-8");
        let _guard = EnvGuard::set("MINICODING_HOME", home_str);
        let home = Utf8PathBuf::from(home_str);

        // 各子路径应在 home 下，且相对位置正确
        assert_eq!(sessions_dir().expect("sessions_dir"), home.join("sessions"));
        assert_eq!(
            config_path().expect("config_path"),
            home.join("config.toml")
        );
        assert_eq!(
            last_known_good_path().expect("last_known_good_path"),
            home.join(".last-known-good.toml")
        );
        assert_eq!(
            audit_log_path().expect("audit_log_path"),
            home.join("audit.log")
        );
        assert_eq!(memory_dir().expect("memory_dir"), home.join("memory"));
        assert_eq!(
            mcp_choices_path().expect("mcp_choices_path"),
            home.join("mcp_choices.toml")
        );
    }

    #[test]
    fn all_paths_are_utf8() {
        let _g = lock_env();
        let dir = tempfile::tempdir().expect("创建临时目录");
        let home_str = dir.path().to_str().expect("tempdir 路径应为 UTF-8");
        let _guard = EnvGuard::set("MINICODING_HOME", home_str);

        // 所有路径函数返回 Utf8PathBuf，类型层面已保证 UTF-8；
        // 此处显式断言 as_str() 可成功调用，覆盖回归。
        let _ = minicoding_home().expect("home").as_str().to_string();
        let _ = sessions_dir().expect("sessions").as_str().to_string();
        let _ = config_path().expect("config").as_str().to_string();
        let _ = last_known_good_path().expect("lkg").as_str().to_string();
        let _ = audit_log_path().expect("audit").as_str().to_string();
        let _ = memory_dir().expect("memory").as_str().to_string();
        let _ = mcp_choices_path()
            .expect("mcp_choices")
            .as_str()
            .to_string();
    }

    #[test]
    fn sessions_dir_changes_with_env() {
        let _g = lock_env();
        // 第一次设置
        let dir1 = tempfile::tempdir().expect("创建临时目录 1");
        let home1 = dir1.path().to_str().expect("utf8");
        let _guard1 = EnvGuard::set("MINICODING_HOME", home1);
        let s1 = sessions_dir().expect("sessions_dir 1");
        assert_eq!(s1, Utf8PathBuf::from(home1).join("sessions"));

        // 切换到另一个目录，验证路径随之变化（Drop guard 在 scope 结束时恢复）
        let dir2 = tempfile::tempdir().expect("创建临时目录 2");
        let home2 = dir2.path().to_str().expect("utf8");
        let _guard2 = EnvGuard::set("MINICODING_HOME", home2);
        let s2 = sessions_dir().expect("sessions_dir 2");
        assert_eq!(s2, Utf8PathBuf::from(home2).join("sessions"));
        assert_ne!(s1, s2);
    }
}
