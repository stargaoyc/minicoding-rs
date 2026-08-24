//! `minicoding-desktop` 二进制入口（仅 `desktop` feature 启用时编译）。
//!
//! 启动 Tauri WebView，注册 invoke 命令供前端调用：
//! - `start_session`：启动 sidecar，返回端口
//! - `get_provider_config` / `save_provider_config`：读写 `config.toml` 的 provider 配置
//! - `store_api_key` / `load_api_key` / `delete_api_key`：OS keyring 凭证管理
//! - `open_config_file`：用系统编辑器打开配置文件
//! - `open_workspace_file`：用系统默认编辑器打开工作区文件（W-11）
//! - `select_workspace_dir`：原生目录选择器（W-11 新建会话先选工作目录）
//!
//! 同时初始化系统托盘 + 全局快捷键（W-07）。
//! 需要系统 webview 运行时（`webkit2gtk` Linux / `WebKit` macOS / `WebView2` Windows）。

// Windows 下以 GUI 子系统编译（2026-08-23 用户反馈）：否则启动时除应用窗外
// 还会弹出一个承载 stdout/stderr 日志的控制台窗口。日志统一改写
// `<安装目录>/logs/`（见下方 log dir 与 plugin targets），开发调试读文件即可。
#![windows_subsystem = "windows"]
#![deny(clippy::all, clippy::pedantic)]

use minicoding_core::config::ProviderConfig;
use minicoding_desktop::{config, sidecar, tray};
use tauri::Manager;

/// 日志目录：`<安装目录>/logs/`（exe 同级，2026-08-23 用户反馈——日志不再
/// 弹控制台窗口/散落 temp）。取 exe 所在目录；获取失败回退系统 temp。
fn resolve_log_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("logs")))
        .unwrap_or_else(std::env::temp_dir)
}

/// panic 日志文件名（位于 [`resolve_log_dir`] 目录）。
const PANIC_LOG_FILE: &str = "minicoding-panic.log";

/// 将 panic 信息直接写入临时文件（不依赖 log crate，确保 logger 未初始化时也能记录）。
///
/// Windows 双击启动时 stderr 不可见，若 panic 仅输出到 stderr 则用户无法诊断。
/// 此函数将 panic 信息追加写入 `%TEMP%\\minicoding-panic.log`（或 `/tmp/minicoding-panic.log`）。
fn write_panic_to_file(location: &str, payload: &str) {
    use std::io::Write;
    let timestamp = chrono_like_timestamp();

    // 获取系统临时目录
    let mut log_path = resolve_log_dir();
    let _ = std::fs::create_dir_all(&log_path);
    log_path.push(PANIC_LOG_FILE);

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
    let app_builder = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                // 过滤 DEBUG 噪音（tao event loop / keyring / tauri::manager 的
                // 内部调试日志）；业务日志从 INFO 起
                .level(log::LevelFilter::Info)
                .targets([
                    // Webview 目标仅转发到前端 devtools（不产生窗口），保留
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Webview),
                    // 日志写安装目录 logs/（2026-08-23 用户反馈：不再弹控制台/
                    // 不散落系统 LogDir；GUI 子系统下 Stdout 无处输出，移除）
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Folder {
                        path: resolve_log_dir(),
                        file_name: Some("minicoding".to_string()),
                    }),
                ])
                .build(),
        )
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            start_session,
            get_provider_config,
            save_provider_config,
            get_config_revision,
            get_context_config,
            save_context_config,
            store_api_key,
            load_api_key,
            delete_api_key,
            open_config_file,
            open_workspace_file,
            select_workspace_dir,
            restart_app,
        ])
        .setup(|app| {
            log::info!(
                "minicoding-desktop 启动中… (version: {})",
                env!("CARGO_PKG_VERSION")
            );
            // sidecar 进程句柄 state（退出时 kill 用，见 `RunEvent::Exit` 处理）
            app.manage(sidecar::SidecarProcess::default());
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
        });

    // 用 `build` + `App::run(callback)` 而非 `Builder::run` 的简写形式：
    // 需要在 `RunEvent::Exit` 时终止 sidecar 进程（tauri-plugin-shell 的
    // `CommandChild` 无 Drop 清理，退出不 kill 则 sidecar 变孤儿进程）。
    let app_result = app_builder.build(tauri::generate_context!());
    let app = match app_result {
        Ok(app) => app,
        Err(e) => {
            let msg = format!("Tauri 应用启动失败: {e}");
            eprintln!("{msg}");
            log::error!("{msg}");
            // 写入 panic 日志文件（确保 stderr 不可见时也能诊断）
            write_panic_to_file("tauri::Builder::build", &msg);
            // 尝试弹出 native 错误对话框（Windows 下用户双击启动时 stderr 不可见）
            show_error_dialog("minicoding 启动失败", &msg);
            return;
        }
    };
    app.run(|handle, event| {
        // 应用退出（托盘"退出"、`restart_app` 重启）时清理 sidecar 进程。
        // 窗口关闭只是隐藏到托盘（prevent_close），不触发 Exit，sidecar 保持运行。
        if let tauri::RunEvent::Exit = event {
            sidecar::kill_sidecar(handle);
        }
    });
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

/// `save_provider_config`：保存 provider 配置到 `config.toml`（原子写入，M-10 防陈旧写）。
///
/// `api_key` 字段不落明文，由 `store_api_key` 写入 OS keyring（C-04）。
/// `expected_revision` 为 `None` 时无条件写（兼容旧前端）；为 `Some(x)` 时 revision
/// 不匹配返回 `StaleWrite` 错误，前端需刷新后重试。
#[tauri::command]
fn save_provider_config(
    provider: ProviderConfig,
    expected_revision: Option<u64>,
) -> Result<(), String> {
    config::save_provider_config(provider, expected_revision).map_err(|e| e.to_string())
}

/// `get_config_revision`：读取当前配置修订号（前端保存前锁定基准，M-10 防陈旧写）。
#[tauri::command]
fn get_config_revision() -> Result<u64, String> {
    let c = minicoding_core::config::load_config()?;
    Ok(c.revision)
}

/// `get_context_config`：读取上下文配置（`[context]` 段，turn 超时/压缩开关）。
#[tauri::command]
fn get_context_config() -> Result<minicoding_core::config::ContextConfig, String> {
    config::get_context_config().map_err(|e| e.to_string())
}

/// `save_context_config`：保存上下文配置到 `config.toml`（原子写入，保留其他段）。
#[tauri::command]
fn save_context_config(context: minicoding_core::config::ContextConfig) -> Result<(), String> {
    config::save_context_config(context).map_err(|e| e.to_string())
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

/// `open_workspace_file`：用系统默认编辑器打开工作区文件（W-11）。
///
/// `path` 为**绝对路径**（前端由工作区 root + 相对路径拼接；桌面端
/// sidecar 的 workdir 可能与桌面进程 CWD 不一致，相对路径不可靠）。
/// 走 `tauri-plugin-shell` 的 `open`（与 `open_config_file` 一致），
/// 不经前端权限链路（系统编辑器打开文件由用户桌面会话授权）。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command 签名要求 AppHandle 按值传递
fn open_workspace_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_shell::ShellExt;
    #[allow(deprecated)] // 同 open_config_file：暂未引入 tauri-plugin-opener
    app.shell()
        .open(&path, None)
        .map_err(|e| format!("打开文件失败: {e}"))
}

/// `select_workspace_dir`：原生目录选择器（W-11 新建会话先选工作目录）。
///
/// 用户取消时返回 `Ok(None)`（前端回退到默认目录）。选中的目录将作为
/// `POST /sessions` 的 `workdir` 传给 sidecar（同机路径，无需转换）。
/// `pick_folder` 是回调 API（对话框在系统 UI 线程弹出），用 oneshot 桥接
/// 为 async 返回值（不阻塞 command 的 runtime 线程）。
#[tauri::command]
async fn select_workspace_dir(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("选择工作目录")
        .pick_folder(move |path| {
            // `into_path` 返回 `Result<PathBuf>`（非 URL 路径时 Ok，桌面文件夹选择恒为本地路径）
            let picked = path.and_then(|p| {
                p.into_path()
                    .ok()
                    .map(|pb| pb.to_string_lossy().to_string())
            });
            let _ = tx.send(picked);
        });
    rx.await.map_err(|_| "目录选择器未返回结果".to_string())
}

/// `restart_app`：重启应用（编辑模式保存配置后调用）。///
/// Tauri `AppHandle::restart()` 会重启当前进程；`RunEvent::Exit` 处理中
/// 会 kill 旧 sidecar（见 `app.run` 回调），新进程启动后由前端重新
/// `start_session` 拉起新 sidecar。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri command 签名要求 AppHandle 按值传递
fn restart_app(app: tauri::AppHandle) {
    log::info!("用户请求重启应用以应用新 sidecar 配置");
    app.restart();
}
