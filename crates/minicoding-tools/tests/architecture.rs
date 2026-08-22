//! 架构守卫（A8，AGENTS.md §3.2）：领域 crate 只准依赖白名单内的 workspace 成员。
#[test]
fn workspace_deps_stay_within_whitelist() {
    minicoding_core::testing::manifest_guard::assert_workspace_deps(&[
        "minicoding-core",
        "minicoding-policy",
    ]);
}
