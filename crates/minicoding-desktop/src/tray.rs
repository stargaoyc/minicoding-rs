//! 系统托盘 + 全局快捷键（W-07，feature gate `desktop`）。
//!
//! - **系统托盘**：显示应用图标，右键菜单含"显示窗口"/"退出"；
//! - **全局快捷键**：`Cmd/Ctrl+Shift+M` 切换窗口显示/隐藏。
//!
//! 详见 `docs/design.md` §26.5、`docs/features.md` W-07。

#![cfg(feature = "desktop")]

use anyhow::Result;
use tauri::{
    AppHandle, Manager, WebviewWindow,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};

/// 初始化系统托盘与全局快捷键（在 Tauri `setup` 中调用）。
///
/// # Errors
/// 托盘或菜单创建失败时返回错误。
pub fn init(app: &AppHandle) -> Result<()> {
    setup_tray(app)?;
    setup_global_shortcut(app)?;
    Ok(())
}

/// 创建系统托盘图标 + 右键菜单。
fn setup_tray(app: &AppHandle) -> Result<()> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::new()
        .icon(app.default_window_icon().cloned().unwrap())
        .tooltip("minicoding")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(window) = main_window(app) {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    tracing::info!("系统托盘已初始化");
    Ok(())
}

/// 注册全局快捷键 `Cmd/Ctrl+Shift+M` 切换窗口显示/隐藏。
fn setup_global_shortcut(app: &AppHandle) -> Result<()> {
    use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};

    let shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyM);

    app.global_shortcut()
        .on_shortcut(shortcut, move |app, _event| {
            if let Some(window) = main_window(app) {
                if window.is_visible().unwrap_or(false) {
                    let _ = window.hide();
                } else {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })?;

    tracing::info!("全局快捷键已注册: Cmd/Ctrl+Shift+M");
    Ok(())
}

/// 获取主窗口。
fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("main")
}
