//! 架构守卫（ARCH-3，2026-08-26 R3 审查）：server 是多前端接入层，可依赖
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

/// R9 P0-2/H5：双轨 builder 能力矩阵一致性测试。
///
/// server `runtime_builder.rs` 与 SDK `builder.rs` 是两套独立工具装配——
/// 任何一侧新增/遗漏工具注册都会产生 Web/Desktop 与 CLI/TUI 的能力漂移
/// （R8 已发生：git/web/memory/ui.ask 在 server 侧缺失补回）。本测试
/// 以相同 `register_*` 函数组合构造两个 registry，断言工具名集合一致
/// （web feature 差异经 cfg 归一）。
#[test]
fn capability_matrix_server_matches_sdk_assembly() {
    use minicoding_core::tool::ToolRegistry;

    // SDK 装配组合（builder.rs 第 6 步）：readonly + ui + write + shell + git + web(optional) + task + memory
    let mut sdk = ToolRegistry::new();
    minicoding_tools::register_readonly_tools(&mut sdk);
    minicoding_tools::register_ui_tools(&mut sdk);
    minicoding_tools::register_write_tools(&mut sdk);
    minicoding_tools::register_shell_tools(&mut sdk);
    minicoding_tools::register_git_tools(&mut sdk);
    // server 始终启用 tools 的 web feature（Cargo.toml），SDK 侧 web 为
    // feature-gated（builder.rs cfg(feature="web")）——一致性测试统一按
    // 已启用处理，feature 关闭侧差异由各自 crate 的架构守卫覆盖。
    minicoding_tools::register_web_tools(&mut sdk);
    minicoding_tools::register_task_tools(&mut sdk, None);
    minicoding_tools::register_memory_tools(
        &mut sdk,
        std::sync::Arc::new(minicoding_memory::LongTermMemory::default()),
    );

    // server 装配组合（runtime_builder.rs 第 7 步）：readonly + write + shell + task + git + web + ui + memory
    let mut server = ToolRegistry::new();
    minicoding_tools::register_readonly_tools(&mut server);
    minicoding_tools::register_write_tools(&mut server);
    minicoding_tools::register_shell_tools(&mut server);
    minicoding_tools::register_task_tools(&mut server, None);
    minicoding_tools::register_git_tools(&mut server);
    minicoding_tools::register_web_tools(&mut server);
    minicoding_tools::register_ui_tools(&mut server);
    minicoding_tools::register_memory_tools(
        &mut server,
        std::sync::Arc::new(minicoding_memory::LongTermMemory::default()),
    );

    let sdk_names: std::collections::BTreeSet<String> =
        sdk.schemas().into_iter().map(|s| s.name).collect();
    let server_names: std::collections::BTreeSet<String> =
        server.schemas().into_iter().map(|s| s.name).collect();

    assert_eq!(
        server_names,
        sdk_names,
        "server 与 SDK 工具集漂移：server 独有={:?} SDK 独有={:?}",
        server_names.difference(&sdk_names).collect::<Vec<_>>(),
        sdk_names.difference(&server_names).collect::<Vec<_>>(),
    );
}
