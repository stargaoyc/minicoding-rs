//! workspace 依赖方向守卫（A8，AGENTS.md §3.2）。
//!
//! 各领域 crate 在自己的 `tests/architecture.rs` 中调用 [`assert_workspace_deps`]，
//! 断言其 `[dependencies]` 不含白名单外的 `minicoding-*` crate——把"领域互不依赖"
//! 从文档约束升级为 CI 强制。只检查 workspace 内部边；平台/外部重依赖由各 crate
//! 的 feature gate 治理（见 modules.md §0.4），不在本守卫范围。

/// 断言当前 crate（以测试 cwd = crate 根为准）的 `[dependencies]` 中，
/// 所有 `minicoding-*` 依赖都在 `allowed` 白名单内。
///
/// # Panics
/// 存在白名单外的 workspace 依赖时 panic（CI 门禁语义）。
pub fn assert_workspace_deps(allowed: &[&str]) {
    let manifest = std::fs::read_to_string("Cargo.toml").expect("读取 Cargo.toml");
    let doc: toml::Value = toml::from_str(&manifest).expect("解析 Cargo.toml");
    let deps = doc
        .get("dependencies")
        .and_then(|v| v.as_table())
        .expect("[dependencies] 必须存在");
    for name in deps.keys() {
        if !name.starts_with("minicoding-") {
            continue; // 只管 workspace 内部依赖边
        }
        assert!(
            allowed.contains(&name.as_str()),
            "架构守卫违规：workspace 依赖 `{name}` 不在白名单 {allowed:?}（AGENTS.md §3.2 领域互不依赖）"
        );
    }
}
