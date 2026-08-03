//! `minicoding-desktop` 二进制入口（仅 `desktop` feature 启用时编译）。
//!
//! 启动 Tauri WebView，注册 `start_session` 命令供前端 `invoke` 调用。
//! 同时初始化系统托盘 + 全局快捷键（W-07）。
//! 需要系统 webview 运行时（`webkit2gtk` Linux / `WebKit` macOS / `WebView2` Windows）。

#![deny(clippy::all, clippy::pedantic)]

use minicoding_desktop::{sidecar, tray};
use tauri::Manager;

/// Tauri 应用入口。
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![start_session_handler])
        .setup(|app| {
            // W-07：初始化系统托盘 + 全局快捷键
            if let Err(e) = tray::init(app.handle()) {
                tracing::warn!(error = %e, "系统托盘/全局快捷键初始化失败（非致命）");
            }
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
        .expect("Tauri 应用启动失败");
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
