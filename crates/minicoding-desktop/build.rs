// Tauri build script（仅 desktop feature 启用时运行 tauri_build）。
//
// `tauri::generate_context!()` 宏依赖 build script 设置的 `OUT_DIR` 环境变量。
// 当 desktop feature 启用时，调用 `tauri_build::build()` 生成 Tauri 上下文。

fn main() {
    // 仅在 desktop feature 启用时运行 tauri_build（条件编译移除未启用时的引用）
    #[cfg(feature = "desktop")]
    {
        tauri_build::build();
    }
}
