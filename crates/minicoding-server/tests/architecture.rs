//! 架构守卫（ARCH-3，2026-08-26 R3 审查）：server 是多前端接入层，可依赖
//! core/protocol/实现 crate 与 tools 组合层；不依赖 cli/tui/sdk/desktop。
//! `minicoding-sdk` 仅出现在 dev-dependencies（R10-05：能力矩阵测试调用真实
//! 装配函数比对），不构成生产依赖。
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
        // R10-05：dev-only（测试调用 SDK 真实装配函数比对；生产依赖图不含）
        "minicoding-sdk",
    ]);
}

/// R9 P0-2/H5 + R10-05：双轨 builder 能力矩阵一致性测试。
///
/// server `runtime_builder.rs` 与 SDK `builder.rs` 是两套独立工具装配——
/// 任何一侧新增/遗漏工具注册都会产生 Web/Desktop 与 CLI/TUI 的能力漂移
/// （R8 已发生：git/web/memory/ui.ask 在 server 侧缺失补回）。
///
/// **R10-05 修复**：此前测试把两侧 `register_*` 序列各自硬编码在测试体内、
/// 与生产代码无关（恒真）——从生产代码删除任何注册测试仍绿。现改为调用
/// 真实装配函数 `assemble_server_tool_registry` / `assemble_sdk_tool_registry`
/// （两者与生产 `build_runtime` 共用），任何一侧增删注册立即红灯。
#[test]
fn capability_matrix_server_matches_sdk_assembly() {
    // SDK 装配（真实生产函数）：readonly + ui + write + shell + git + web + task + memory
    let event_bus_sdk = minicoding_core::runtime::EventBus::new();
    let sdk = minicoding_sdk::builder::assemble_sdk_tool_registry(
        &event_bus_sdk,
        std::sync::Arc::new(minicoding_memory::LongTermMemory::default()),
        std::sync::Arc::new(minicoding_memory::AutoMemory::default()),
    );

    // server 装配（真实生产函数）：readonly + write + shell + task + git + web + ui + memory
    let event_bus_server = minicoding_core::runtime::EventBus::new();
    let server =
        minicoding_server::runtime_builder::assemble_server_tool_registry(&event_bus_server);

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
