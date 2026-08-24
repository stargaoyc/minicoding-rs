//! sidecar 进程管理：启动 `minicoding-server` 作为子进程，读取监听端口。
//!
//! 详见 `docs/design.md` §26.5。
//!
//! `desktop` feature 启用时额外提供 Tauri `AppHandle` 版本（通过 `tauri-plugin-shell`
//! 的 sidecar API）；未启用时 `spawn_sidecar_standalone` 用 `tokio::process` 直接启动。

use crate::SessionInfo;
use anyhow::{Context, Result};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

// `app.state::<T>()` / `app.manage(...)` 来自 `tauri::Manager`（main.rs 已导入，
// 本文件独立使用需显式引入；CI 的 desktop feature build 会因缺失而失败）。
#[cfg(feature = "desktop")]
use tauri::Manager;

/// sidecar 启动后等待端口输出的超时时间。
const SIDECAR_TIMEOUT: Duration = Duration::from_secs(10);

/// 从 `config.toml` 构造 provider 非敏感配置的 CLI 参数。
///
/// 读 `~/.minicoding/config.toml` 的 `[provider]` 段，返回 `--provider`/`--provider-name`/
/// `--api-base`/`--model` 参数。**不传 `--api-key`**（C-04：API key 由 sidecar 自己读
/// keyring，不通过参数/env 传递，避免 `/proc/<pid>/cmdline` 泄露凭证）。
fn build_provider_args_from_config() -> Vec<String> {
    let mut args = Vec::new();
    match crate::config::get_provider_config() {
        Ok(provider) => {
            args.push("--provider".into());
            args.push(provider.default);
            if let Some(name) = provider.name {
                args.push("--provider-name".into());
                args.push(name);
            }
            if !provider.api_base.is_empty() {
                args.push("--api-base".into());
                args.push(provider.api_base);
            }
            if !provider.model.is_empty() {
                args.push("--model".into());
                args.push(provider.model);
            }
        }
        Err(e) => {
            log::warn!("读取 provider 配置失败，sidecar 用默认值: {e}");
        }
    }
    args
}

/// 启动 `minicoding-server` sidecar 并返回监听端口（独立版本，无 Tauri 依赖）。
///
/// sidecar 命令：`minicoding-server --bind 127.0.0.1:0 [--web <dir>] [--provider ...]`
/// 启动后在 stdout 输出实际监听端口（如 `listening on 127.0.0.1:12345`）。
///
/// provider 非敏感配置（`api_base`/`model`/`name`）从 `config.toml` 读取并通过 CLI 参数传递；
/// API key 由 sidecar 自己读 keyring（C-04，不通过参数/env 传）。
///
/// # Errors
/// - sidecar 二进制未找到；
/// - 启动超时（10s 内未输出端口）；
/// - 端口解析失败。
pub async fn spawn_sidecar_standalone() -> Result<SessionInfo> {
    let bin =
        std::env::var("MINICODING_SERVER_BIN").unwrap_or_else(|_| "minicoding-server".to_string());

    let web_dir = crate::resolve_web_dir();
    // S1：desktop 生成 token 并经 CLI 参数传给 sidecar（内存传递，C-04 兼容）
    let token = minicoding_core::util::generate_auth_token();
    let mut cmd = Command::new(&bin);
    cmd.args(["--bind", "127.0.0.1:0", "--auth-token", &token]);
    // 注入 provider 非敏感配置（从 config.toml 读取）
    cmd.args(build_provider_args_from_config());
    if let Some(dir) = &web_dir {
        cmd.args(["--web", dir.as_str()]);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Windows：sidecar 是控制台子系统程序，不加此标志会随 GUI 主程序弹出
    // 第二个控制台窗口（tauri shell sidecar 路径由插件内置同款标志）
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

    let mut child = cmd
        .spawn()
        .with_context(|| format!("启动 sidecar 失败: {bin}"))?;
    let pid = child.id().context("无法获取 sidecar PID")?;

    let stdout = child.stdout.take().context("sidecar stdout 不可用")?;
    let mut reader = BufReader::new(stdout).lines();

    let port = tokio::time::timeout(SIDECAR_TIMEOUT, async {
        while let Ok(Some(line)) = reader.next_line().await {
            log::info!("sidecar stdout: {line}");
            if let Some(p) = parse_port(&line) {
                return Ok(p);
            }
        }
        anyhow::bail!("sidecar 未输出端口信息")
    })
    .await
    .context("sidecar 启动超时")??;

    log::info!("sidecar 已启动: port={port}, pid={pid}");
    Ok(SessionInfo { port, pid, token })
}

/// 从 sidecar 输出行解析监听端口。
///
/// 支持格式：
/// - `listening on 127.0.0.1:12345`
/// - `minicoding-server 启动`（tracing 日志含 addr 字段）
/// - `addr=127.0.0.1:12345`
/// - `port: 3000 ready`（冒号后允许空白）
#[must_use]
fn parse_port(line: &str) -> Option<u16> {
    let line = line.trim();
    let idx = line.rfind(':')?;
    let rest = line[idx + 1..].trim_start();
    let port_str: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let p = port_str.parse::<u16>().ok()?;
    (1024..=65535).contains(&p).then_some(p)
}

// ─── Tauri 集成（feature gate `desktop`）────────────────────────────────────

/// 全局 sidecar 进程句柄（Tauri 版 sidecar 的 `CommandChild`）。
///
/// `tauri-plugin-shell` 的 `CommandChild` **没有 Drop 清理**（无 `kill_on_drop`），
/// 若退出时不显式 kill，sidecar 变孤儿进程继续存活（用户反馈
/// "minicoding-server-sidecar.exe 不随 desktop 退出停止"）。句柄存入 managed
/// state，应用 `RunEvent::Exit` 时由 [`kill_sidecar`] 取出并终止。
#[cfg(feature = "desktop")]
pub struct SidecarProcess(pub std::sync::Mutex<Option<tauri_plugin_shell::process::CommandChild>>);

#[cfg(feature = "desktop")]
impl Default for SidecarProcess {
    fn default() -> Self {
        Self(std::sync::Mutex::new(None))
    }
}

/// 终止 sidecar 进程（应用退出/重启时调用）。
///
/// `CommandChild::kill` 消费自身（从 state 中 `take` 后调用），幂等：未启动过
/// sidecar 或已 kill 过时无操作。
#[cfg(feature = "desktop")]
pub fn kill_sidecar(app: &tauri::AppHandle) {
    let state = app.state::<SidecarProcess>();
    let Some(child) = state
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        return;
    };
    match child.kill() {
        Ok(()) => log::info!("sidecar 已终止（应用退出清理）"),
        Err(e) => log::warn!("sidecar 终止失败: {e}"),
    }
}

/// 启动 sidecar（Tauri 版本，通过 `tauri-plugin-shell` sidecar API）。
///
/// 仅 `desktop` feature 启用时可用。生产模式下 sidecar 二进制通过
/// `tauri.conf.json` 的 `externalBin` 打包。
///
/// # Errors
///
/// - sidecar 二进制配置错误（`tauri.conf.json` 的 `externalBin` 未正确设置）
/// - sidecar 进程启动失败（二进制缺失或权限不足）
/// - 端口解析失败（sidecar stdout 未输出 `PORT=` 行或格式错误）
#[cfg(feature = "desktop")]
pub async fn spawn_sidecar(app: &tauri::AppHandle) -> Result<SessionInfo> {
    use tauri_plugin_shell::ShellExt;
    use tauri_plugin_shell::process::CommandEvent;

    let sidecar = app
        .shell()
        .sidecar("minicoding-server-sidecar")
        .map_err(|e| anyhow::anyhow!("sidecar 配置错误: {e}"))?;

    let web_dir = crate::resolve_web_dir();
    // S1：同 standalone 路径——token 内存传递（desktop 生成，前端请求时携带）
    let token = minicoding_core::util::generate_auth_token();
    let mut args: Vec<String> = vec![
        "--bind".into(),
        "127.0.0.1:0".into(),
        "--auth-token".into(),
        token.clone(),
    ];
    // 注入 provider 非敏感配置（从 config.toml 读取，API key 由 sidecar 读 keyring）
    args.extend(build_provider_args_from_config());
    if let Some(dir) = &web_dir {
        args.push("--web".into());
        args.push(dir.to_string());
    }

    let (mut rx, child) = sidecar
        .args(&args)
        .spawn()
        .map_err(|e| anyhow::anyhow!("sidecar 启动失败: {e}"))?;
    let pid = child.pid();

    // 保存 child 句柄到 managed state：`CommandChild` 无 Drop 清理，退出时
    // 由 `RunEvent::Exit` → `kill_sidecar` 显式终止（防孤儿进程）。
    {
        let state = app.state::<SidecarProcess>();
        *state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(child);
    }

    let port = tokio::time::timeout(SIDECAR_TIMEOUT, async {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line_bytes) => {
                    let line = String::from_utf8_lossy(&line_bytes);
                    log::info!("sidecar stdout: {}", line.trim());
                    if let Some(p) = parse_port(&line) {
                        return Ok(p);
                    }
                }
                CommandEvent::Stderr(line_bytes) => {
                    let line = String::from_utf8_lossy(&line_bytes);
                    log::warn!("sidecar stderr: {}", line.trim());
                }
                _ => {}
            }
        }
        anyhow::bail!("sidecar 未输出端口信息")
    })
    .await
    .context("sidecar 启动超时")??;

    // 端口解析后继续捕获 sidecar 日志（后台 task，避免服务器错误不可见）
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line_bytes) => {
                    log::info!("sidecar: {}", String::from_utf8_lossy(&line_bytes).trim());
                }
                CommandEvent::Stderr(line_bytes) => {
                    log::warn!("sidecar: {}", String::from_utf8_lossy(&line_bytes).trim());
                }
                _ => {}
            }
        }
    });

    log::info!("Tauri sidecar 已启动: port={port}, pid={pid}");
    Ok(SessionInfo { port, pid, token })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_port_extracts_valid_port() {
        assert_eq!(parse_port("listening on 127.0.0.1:12345"), Some(12345));
        assert_eq!(parse_port("addr=127.0.0.1:8080"), Some(8080));
        assert_eq!(parse_port("port: 3000 ready"), Some(3000));
    }

    #[test]
    fn parse_port_rejects_invalid() {
        assert_eq!(parse_port("no port here"), None);
        assert_eq!(parse_port("port: 80"), None);
        assert_eq!(parse_port("port: 99999"), None);
    }
}
