//! 系统托盘 + 全局快捷键（W-07，feature gate `desktop`）。
//!
//! - **系统托盘**：显示应用图标，右键菜单含"显示窗口"/"退出"；
//! - **全局快捷键**：`Ctrl+Alt+M` 切换窗口显示/隐藏（原 `Win/Cmd+Shift+M` 在
//!   Windows 上被系统保留为"最小化所有窗口"，注册必然失败，故改用无冲突组合）。
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
/// 托盘失败返回错误（阻塞启动流程）；全局快捷键失败仅降级告警——
/// 部分平台/桌面环境会占用组合键导致注册失败（如 Windows 的 `Win+Shift+M`），
/// 此时托盘菜单仍可完成"显示窗口/退出"，不应让整个初始化失败。
///
/// # Errors
/// 托盘或菜单创建失败时返回错误。
pub fn init(app: &AppHandle) -> Result<()> {
    setup_tray(app)?;
    if let Err(e) = setup_global_shortcut(app) {
        log::warn!("全局快捷键注册失败（非致命，托盘仍可用）: {e}");
    }
    Ok(())
}

/// 创建系统托盘图标 + 右键菜单。
fn setup_tray(app: &AppHandle) -> Result<()> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    // 获取窗口图标，缺失时用空白图标兜底（不 panic）
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("default_window_icon 为 None（图标未嵌入）"))?;

    TrayIconBuilder::new()
        .icon(icon)
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

    log::info!("系统托盘已初始化");
    Ok(())
}

/// 注册全局快捷键 `Ctrl+Alt+M` 切换窗口显示/隐藏。
///
/// 键位选型说明（why）：原 `Win/Cmd+Shift+M` 在 Windows 上被系统保留为
/// "最小化所有窗口"快捷键，注册必失败；`Ctrl+Alt+M` 在 Windows/macOS/Linux
/// 均无系统保留冲突，且不与编辑器常用组合冲突。
fn setup_global_shortcut(app: &AppHandle) -> Result<()> {
    use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};

    let shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyM);

    app.global_shortcut()
        .on_shortcut(shortcut, move |app, _shortcut, _event| {
            if let Some(window) = main_window(app) {
                if window.is_visible().unwrap_or(false) {
                    let _ = window.hide();
                } else {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })?;

    log::info!("全局快捷键已注册: Ctrl+Alt+M");
    Ok(())
}

/// 获取主窗口。
fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("main")
}
