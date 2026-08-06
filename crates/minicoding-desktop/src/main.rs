//! `minicoding-desktop` 二进制入口（仅 `desktop` feature 启用时编译）。
//!
//! 启动 Tauri WebView，注册 invoke 命令供前端调用：
//! - `start_session`：启动 sidecar，返回端口
//! - `get_provider_config` / `save_provider_config`：读写 `config.toml` 的 provider 配置
//! - `store_api_key` / `load_api_key` / `delete_api_key`：OS keyring 凭证管理
//! - `open_config_file`：用系统编辑器打开配置文件
//!
//! 同时初始化系统托盘 + 全局快捷键（W-07）。
//! 需要系统 webview 运行时（`webkit2gtk` Linux / `WebKit` macOS / `WebView2` Windows）。

#![deny(clippy::all, clippy::pedantic)]

use minicoding_core::config::ProviderConfig;
use minicoding_desktop::{config, sidecar, tray};
use tauri::Manager;

/// panic 日志文件名（写入 temp 目录，Windows 下 `%TEMP%\\minicoding-panic.log`）。
const PANIC_LOG_FILE: &str = "minicoding-panic.log";

/// 将 panic 信息直接写入临时文件（不依赖 log crate，确保 logger 未初始化时也能记录）。
///
/// Windows 双击启动时 stderr 不可见，若 panic 仅输出到 stderr 则用户无法诊断。
/// 此函数将 panic 信息追加写入 `%TEMP%\\minicoding-panic.log`（或 `/tmp/minicoding-panic.log`）。
fn write_panic_to_file(location: &str, payload: &str) {
    use std::io::Write;
    let timestamp = chrono_like_timestamp();

    // 获取系统临时目录
    let log_path = std::env::temp_dir().join(PANIC_LOG_FILE);

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(
            file,
            "[{timestamp}] panic at {location}\n  payload: {payload}\n  version: {}\n---",
            env!("CARGO_PKG_VERSION")
        );
    }
}

/// 简单时间戳（避免引入 chrono 依赖，用 `std::time` + 本地格式化）。
fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // 简单格式：Unix 秒数（足够诊断，无需完整日期格式化）
    format!("unix:{secs}")
}

/// Tauri 应用入口。
fn main() {
    // 安装 panic hook：将 panic 信息写入文件 + stderr，便于诊断崩溃。
    // 必须在 Tauri builder 之前安装，确保任何阶段的 panic 都能被捕获。
    // （Tauri plugin-log 在 builder 阶段才初始化，此前 log::error! 是 no-op）
    std::panic::set_hook(Box::new(|info| {
        let location = info.location().map_or_else(
            || "<unknown>".to_string(),
            |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
        );
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());

        eprintln!("panic at {location}: {payload}");

        // 直接写文件（不依赖 log crate，确保 logger 未初始化时也能记录）
        write_panic_to_file(&location, &payload);

        // log crate 可能已初始化（panic 发生在 builder 之后），尝试记录
        log::error!("应用 panic: location={location}, payload={payload}");
    }));

    // 启动 Tauri 应用，失败时弹出错误对话框（Windows 下 stderr 不可见）
    let run_result = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Webview),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("minicoding".to_string()),
                    }),
                ])
                .build(),
        )
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            start_session,
            get_provider_config,
            save_provider_config,
            store_api_key,
            load_api_key,
            delete_api_key,
            open_config_file,
            restart_app,
        ])
        .setup(|app| {
            log::info!(
                "minicoding-desktop 启动中… (version: {})",
                env!("CARGO_PKG_VERSION")
            );
            // W-07：初始化系统托盘 + 全局快捷键（失败非致命，不阻塞启动）
            if let Err(e) = tray::init(app.handle()) {
                log::warn!("系统托盘/全局快捷键初始化失败（非致命）: {e}");
            }
            log::info!("minicoding-desktop 启动完成");
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭窗口时隐藏到托盘而非退出（保持 sidecar 运行）
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && let Some(main_window) = window.get_webview_window("main")
            {
                let _ = main_window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!());

    if let Err(e) = run_result {
        let msg = format!("Tauri 应用启动失败: {e}");
        eprintln!("{msg}");
        log::error!("{msg}");

        // 写入 panic 日志文件（确保 stderr 不可见时也能诊断）
        write_panic_to_file("tauri::Builder::run", &msg);

        // 尝试弹出 native 错误对话框（Windows 下用户双击启动时 stderr 不可见）
        // 若对话框也失败，至少文件已写入，用户可查看 %TEMP%\minicoding-panic.log
        show_error_dialog("minicoding 启动失败", &msg);
    }
}

/// 跨平台弹出 native 错误对话框（阻塞直到用户关闭）。
///
/// - Windows: PowerShell `MessageBox.Show()`（阻塞等待用户点击 OK）
/// - macOS: `osascript display dialog`（阻塞等待用户点击 OK）
/// - Linux: `zenity --error`（阻塞等待用户点击 OK）
///
/// **必须用 `status()` 而非 `spawn()`**：`spawn()` 不等待子进程，主进程可能在
/// 对话框显示前就退出，用户看不到错误。`status()` 阻塞直到对话框关闭。
///
/// 此函数 intentionally 不返回 Result —— 对话框失败不应影响错误日志已写入文件。
fn show_error_dialog(title: &str, message: &str) {
    #[cfg(target_os = "windows")]
    {
        // Windows: 用 PowerShell 弹出 MessageBox（阻塞等待用户确认）
        let ps_cmd = format!(
            "Add-Type -AssemblyName PresentationFramework; [System.Windows.MessageBox]::Show('{message}', '{title}', 'OK', 'Error')",
            message = message.replace('\'', "''"),
            title = title.replace('\'', "''")
        );
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps_cmd])
            .status();
    }

    #[cfg(target_os = "macos")]
    {
        // macOS: 用 osascript 弹出对话框（阻塞等待用户确认）
        let script = format!(
            "display dialog \"{message}\" with title \"{title}\" buttons {{\"OK\"}} default button \"OK\" with icon stop",
            message = message.replace('"', "\\\""),
            title = title.replace('"', "\\\"")
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .status();
    }

    #[cfg(target_os = "linux")]
    {
        // Linux: 用 zenity 弹出错误对话框（阻塞等待用户确认）
        // zenity 不存在时 status() 返回 Err，静默忽略（日志已写入文件）
        let _ = std::process::Command::new("zenity")
            .args(["--error", "--title", title, "--text", message])
            .status();
    }

    // 非 Windows/macOS/Linux 平台：仅 stderr + 文件（已由调用方处理）
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = (title, message);
    }
}

/// `start_session` Tauri 命令（`invoke('start_session')`）。
///
/// 前端调用此命令获取 sidecar 端口，然后用 `fetch` + `EventSource` 连接
/// `http://127.0.0.1:PORT`。失败时返回错误，前端显示错误界面。
#[tauri::command]
async fn start_session(app: tauri::AppHandle) -> Result<minicoding_desktop::SessionInfo, String> {
    sidecar::spawn_sidecar(&app).await.map_err(|e| {
        let err_str = e.to_string();
        log::error!("sidecar 启动失败: {err_str}");
        err_str
    })
}

/// `get_provider_config`：读取 provider 配置（`config.toml`）。
#[tauri::command]
fn get_provider_config() -> Result<ProviderConfig, String> {
    config::get_provider_config().map_err(|e| e.to_string())
}

/// `save_provider_config`：保存 provider 配置到 `config.toml`（原子写入）。
///
/// `api_key` 字段不落明文，由 `store_api_key` 写入 OS keyring（C-04）。
#[tauri::command]
fn save_provider_config(provider: ProviderConfig) -> Result<(), String> {
    config::save_provider_config(provider).map_err(|e| e.to_string())
}

/// `store_api_key`：写入 API key 到 OS keyring（与 CLI `cred store` 共享 entry）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command 参数按值传递（JSON 反序列化）
fn store_api_key(api_key: String) -> Result<(), String> {
    config::store_api_key(&api_key).map_err(|e| e.to_string())
}

/// `load_api_key`：从 OS keyring 读取 API key（`Ok(None)` 表示未设置）。
#[tauri::command]
fn load_api_key() -> Result<Option<String>, String> {
    config::load_api_key().map_err(|e| e.to_string())
}

/// `delete_api_key`：删除 keyring 中的 API key。
#[tauri::command]
fn delete_api_key() -> Result<(), String> {
    config::delete_api_key().map_err(|e| e.to_string())
}

/// `open_config_file`：用系统默认编辑器打开 `~/.minicoding/config.toml`。
///
/// 调用 `tauri-plugin-shell` 的 `open` 打开配置文件所在目录（跨平台安全）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command 签名要求 AppHandle 按值传递
fn open_config_file(app: tauri::AppHandle) -> Result<String, String> {
    use tauri_plugin_shell::ShellExt;
    let path = config::config_file_path().map_err(|e| e.to_string())?;
    let dir = path.parent().unwrap_or_else(|| camino::Utf8Path::new("."));
    // tauri-plugin-shell 的 `open` 已 deprecated（建议 tauri-plugin-opener），
    // 但本项目未引入 opener plugin，暂用 shell open（功能正常）。
    #[allow(deprecated)]
    app.shell()
        .open(dir.as_str(), None)
        .map_err(|e| format!("打开配置目录失败: {e}"))?;
    Ok(path.to_string())
}

/// `restart_app`：重启应用（编辑模式保存配置后调用）。
///
/// Tauri `AppHandle::restart()` 会重启当前进程，`kill_on_drop` 确保
/// 旧 sidecar 子进程在进程退出时被杀死，新进程启动后读取新配置。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command 签名要求 AppHandle 按值传递
fn restart_app(app: tauri::AppHandle) {
    log::info!("用户请求重启应用以应用新 sidecar 配置");
    app.restart();
}
