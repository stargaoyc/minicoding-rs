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

/// Tauri 应用入口。
fn main() {
    // 安装 panic hook：将 panic 信息写入日志文件，便于诊断崩溃
    // （默认 panic 只输出到 stderr，Windows 双击启动时 stderr 不可见）
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
        log::error!("应用 panic: location={location}, payload={payload}");
    }));

    tauri::Builder::default()
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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            start_session_handler,
            get_provider_config_handler,
            save_provider_config_handler,
            store_api_key_handler,
            load_api_key_handler,
            delete_api_key_handler,
            open_config_file_handler,
        ])
        .setup(|app| {
            log::info!("minicoding-desktop 启动中…");
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
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("Tauri 应用启动失败: {e}");
            log::error!("Tauri 应用启动失败: {e}");
        });
}

/// `start_session` Tauri 命令（`invoke('start_session')`）。
///
/// 前端调用此命令获取 sidecar 端口，然后用 `fetch` + `EventSource` 连接
/// `http://127.0.0.1:PORT`。失败时回退到开发默认端口 8080。
#[tauri::command]
async fn start_session_handler(
    app: tauri::AppHandle,
) -> Result<minicoding_desktop::SessionInfo, String> {
    sidecar::spawn_sidecar(&app)
        .await
        .map_err(|e| e.to_string())
        .or_else(|_| sidecar::fallback_session_info())
}

/// `get_provider_config`：读取 provider 配置（`config.toml`）。
#[tauri::command]
fn get_provider_config_handler() -> Result<ProviderConfig, String> {
    config::get_provider_config().map_err(|e| e.to_string())
}

/// `save_provider_config`：保存 provider 配置到 `config.toml`（原子写入）。
///
/// `api_key` 字段不落明文，由 `store_api_key` 写入 OS keyring（C-04）。
#[tauri::command]
fn save_provider_config_handler(provider: ProviderConfig) -> Result<(), String> {
    config::save_provider_config(provider).map_err(|e| e.to_string())
}

/// `store_api_key`：写入 API key 到 OS keyring（与 CLI `cred store` 共享 entry）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command 参数按值传递（JSON 反序列化）
fn store_api_key_handler(api_key: String) -> Result<(), String> {
    config::store_api_key(&api_key).map_err(|e| e.to_string())
}

/// `load_api_key`：从 OS keyring 读取 API key（`Ok(None)` 表示未设置）。
#[tauri::command]
fn load_api_key_handler() -> Result<Option<String>, String> {
    config::load_api_key().map_err(|e| e.to_string())
}

/// `delete_api_key`：删除 keyring 中的 API key。
#[tauri::command]
fn delete_api_key_handler() -> Result<(), String> {
    config::delete_api_key().map_err(|e| e.to_string())
}

/// `open_config_file`：用系统默认编辑器打开 `~/.minicoding/config.toml`。
///
/// 调用 `tauri-plugin-shell` 的 `open` 打开配置文件所在目录（跨平台安全）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command 签名要求 AppHandle 按值传递
fn open_config_file_handler(app: tauri::AppHandle) -> Result<String, String> {
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
