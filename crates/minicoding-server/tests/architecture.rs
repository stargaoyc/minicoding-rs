//! 架构守卫：server 是多前端接入层，可依赖
//! core/protocol/实现 crate 与 tools 组合层；不依赖 cli/tui/sdk/desktop。
#[test]
fn architecture_deps_whitelist() {
    minicoding_core::testing::manifest_guard::assert_workspace_deps(&[
        "minicoding-core",
        "minicoding-protocol",
        "minicoding-policy",
        "minicoding-tools",
        "minicoding-context",
        "minicoding-storage",
        "minicoding-providers",
        "minicoding-memory",
        "minicoding-journal",
        "minicoding-sandbox",
    ]);
}
