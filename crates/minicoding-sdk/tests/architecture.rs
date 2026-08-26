//! 架构守卫（ARCH-3 补全，2026-08-26 R3 审查）：sdk 是嵌入式组合层，白名单
//! 外的 workspace 边（尤其反向依赖 server/cli/tui/desktop）即违规。
#[test]
fn architecture_deps_whitelist() {
    minicoding_core::testing::manifest_guard::assert_workspace_deps(&[
        "minicoding-core",
        "minicoding-policy",
        "minicoding-tools",
        "minicoding-context",
        "minicoding-storage",
        "minicoding-providers",
        "minicoding-memory",
        "minicoding-hooks",
        "minicoding-journal",
        "minicoding-sandbox",
        "minicoding-extension-sdk",
        "minicoding-mcp",
    ]);
}
