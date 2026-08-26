//! 架构守卫（ARCH-3，2026-08-26 R3 审查）：tui 经 sdk 组装 Runtime，
//! 白名单外的 workspace 依赖即违规。
#[test]
fn architecture_deps_whitelist() {
    minicoding_core::testing::manifest_guard::assert_workspace_deps(&[
        "minicoding-core",
        "minicoding-policy",
        "minicoding-sdk",
        "minicoding-storage",
    ]);
}
