//! 架构守卫（ARCH-3，2026-08-26 R3 审查）：desktop 是 Tauri 桌面壳，
//! 仅允许依赖 core（sidecar 经进程边界与 server 通信，无 Rust 级耦合）。
#[test]
fn architecture_deps_whitelist() {
    minicoding_core::testing::manifest_guard::assert_workspace_deps(&["minicoding-core"]);
}
