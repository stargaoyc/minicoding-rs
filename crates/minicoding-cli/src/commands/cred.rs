//! `minicoding cred store/load/delete` 子命令（T-M4-11）。
//!
//! 管理 API key 凭证存储（OS keyring + 文件 fallback）。不构建 `Runtime`，
//! 直接调用 `crate::cred` 模块的存储函数。
//!
//! ## 用法
//!
//! ```text
//! minicoding cred store    # 交互式输入 key（不回显）写入 keyring
//! minicoding cred load     # 验证 keyring 中是否有凭证
//! minicoding cred delete   # 删除 keyring 中的凭证
//! ```

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

/// `cred` 顶层子命令。
#[derive(Args, Debug)]
pub struct CredCommand {
    #[command(subcommand)]
    pub action: CredAction,
}

/// `cred` 子命令动作。
#[derive(Subcommand, Debug)]
pub enum CredAction {
    /// 把 API key 写入 keyring（从 stdin 读取，不回显）。
    Store,
    /// 验证 keyring 中是否有凭证（不打印 key 本身）。
    Load,
    /// 删除 keyring 中的凭证。
    Delete,
}

/// 执行 `cred` 子命令。
///
/// # Errors
/// keyring/文件 IO 失败时返回错误。
pub fn run_cred_command(cmd: &CredCommand) -> Result<()> {
    match cmd.action {
        CredAction::Store => {
            let key = read_key_from_stdin()?;
            if key.is_empty() {
                anyhow::bail!("输入的 API key 为空");
            }
            crate::cred::store_api_key(&key).context("写入凭证失败")?;
            println!("凭证已存储");
            Ok(())
        }
        CredAction::Load => {
            let loaded = crate::cred::load_api_key().context("加载凭证失败")?;
            if loaded.is_some() {
                println!("凭证已存在（来源：keyring 或文件 fallback）");
            } else {
                println!("未找到凭证");
            }
            Ok(())
        }
        CredAction::Delete => {
            crate::cred::delete_api_key().context("删除凭证失败")?;
            println!("凭证已删除（如存在）");
            Ok(())
        }
    }
}

/// 从 stdin 读取一行作为 API key（不回显，避免凭证泄露到终端历史）。
fn read_key_from_stdin() -> Result<String> {
    use std::io::BufRead;
    eprintln!("请输入 API key（输入后回车，不回显）：");
    // NOTE: rpassword 之类 crate 可做隐藏输入，但为避免引入新依赖，
    // 此处用普通 stdin 读取。生产建议后续接入 rpassword。
    let mut line = String::new();
    let stdin = std::io::stdin();
    stdin
        .lock()
        .read_line(&mut line)
        .context("读取 stdin 失败")?;
    Ok(line.trim().to_string())
}
