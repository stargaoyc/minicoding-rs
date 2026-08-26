//! workspace 依赖方向守卫（A8，AGENTS.md §3.2）。
//!
//! 各领域 crate 在自己的 `tests/architecture.rs` 中调用 [`assert_workspace_deps`]，
//! 断言其全部依赖表中不含白名单外的 `minicoding-*` crate——把"领域互不依赖"从
//! 文档约束升级为 CI 强制。dev-deps 与 deps 同权重：测试通道的依赖边同样是真实
//! 编译期耦合，同样会阻碍未来拆分。只检查 workspace 内部边；平台/外部重依赖由
//! 各 crate 的 feature gate 治理（见 modules.md §0.4），不在本守卫范围。
//!
//! ARCH-2（2026-08-26 R3 审查）：扫描**全部** `*dependencies*` 表——含
//! `[target.'cfg(..)'.dependencies]`/`[build-dependencies]` 及其嵌套变体。
//! 此前仅查顶层两张表，任何人往 target-specific 表塞一条越界 workspace 边
//! 守卫即静默通过。

/// 断言当前 crate（以测试 cwd = crate 根为准）的 manifest 中，所有依赖表
/// （顶层 + `target.<cfg>.` 嵌套）里的 `minicoding-*` 依赖都在 `allowed`
/// 白名单内。
///
/// # Panics
/// 存在白名单外的 workspace 依赖时 panic（CI 门禁语义）。
pub fn assert_workspace_deps(allowed: &[&str]) {
    let manifest = std::fs::read_to_string("Cargo.toml").expect("读取 Cargo.toml");
    let doc: toml::Value = toml::from_str(&manifest).expect("解析 Cargo.toml");

    // 收集所有依赖表：顶层 dependencies/dev-dependencies/build-dependencies
    // + 每个 [target."<cfg>"] 子表下的同名表。
    let mut sections: Vec<(String, &toml::Value)> = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(v) = doc.get(key) {
            sections.push((key.to_string(), v));
        }
    }
    if let Some(target) = doc.get("target").and_then(|v| v.as_table()) {
        for (cfg, cfg_table) in target {
            for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(v) = cfg_table.get(key) {
                    sections.push((format!("target.{cfg}.{key}"), v));
                }
            }
        }
    }

    for (section, deps) in &sections {
        let Some(table) = deps.as_table() else {
            continue;
        };
        for name in table.keys() {
            if !name.starts_with("minicoding-") {
                continue; // 只管 workspace 内部依赖边
            }
            assert!(
                allowed.contains(&name.as_str()),
                "架构守卫违规：workspace 依赖 `{name}`（[{section}]）不在白名单 \
                 {allowed:?}（AGENTS.md §3.2 领域互不依赖；ARCH-2：target-specific \
                 与 build-dependencies 表同权重检查）"
            );
        }
    }
}
