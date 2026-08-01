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
