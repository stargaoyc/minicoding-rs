//! 架构守卫（ARCH-3，2026-08-26 R3 审查）：cli 为 frontend 组合层，可依赖
//! 全部实现 crate + sdk/server；禁止的边由白名单反向表达——任何新增依赖必须
//! 显式登记（如未来新增领域 crate 需同步此处）。
#[test]
fn architecture_deps_whitelist() {
    minicoding_core::testing::manifest_guard::assert_workspace_deps(&[
        "minicoding-core",
        "minicoding-sdk",
        "minicoding-server",
        "minicoding-context",
        "minicoding-policy",
        "minicoding-storage",
        "minicoding-providers",
        "minicoding-tools",
        "minicoding-memory",
        "minicoding-hooks",
        "minicoding-journal",
        "minicoding-sandbox",
        "minicoding-mcp",
        "minicoding-extension-sdk",
    ]);
}
