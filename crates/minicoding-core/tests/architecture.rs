//! 架构守卫测试（M-05 step 4）：断言 core 不依赖任何领域 crate。
//!
//! 依赖方向（`docs/modules.md` §0.2）：`core ◄── 领域 crate`。core 一旦引入
//! `minicoding-sandbox`/`minicoding-tools` 等依赖即构成逆向依赖或循环依赖，
//! 编译期无法直接断言，故本测试解析 `Cargo.toml` 做白名单检查。
//!
//! 新增 core 依赖时必须先通过本测试（且满足 AGENTS.md §2.7/§3.5 约束）。

use std::fs;
use std::path::PathBuf;

/// core 允许的依赖白名单（与 `docs/modules.md` §1.4 一致：轻量 + 无平台/网络）。
///
/// `ts-rs` 是 optional feature（`ts`），仅类型导出用，也在白名单内。
const ALLOWED_DEPS: &[&str] = &[
    "tokio",
    "tokio-util",
    "futures",
    "serde",
    "serde_json",
    "toml",
    "tracing",
    "thiserror",
    "camino",
    "uuid",
    "ulid",
    "time",
    "home",
    "semver",
    "notify",
    "ts-rs",
];

/// core 禁止的依赖前缀（领域 crate 命名空间）。
const FORBIDDEN_PREFIXES: &[&str] = &[
    "minicoding-", // 任何 minicoding-* 领域 crate（含 optional）
    "reqwest",
    "landlock",
    "libseccomp",
    "rmcp",
    "ratatui",
    "windows",
];

fn core_manifest() -> PathBuf {
    // 测试运行目录是 crate 根（cargo test 以 crate dir 为 cwd）。
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")
}

#[test]
fn core_dependencies_stay_within_whitelist() {
    let manifest = fs::read_to_string(core_manifest()).expect("读取 core Cargo.toml");
    let doc: toml::Value = toml::from_str(&manifest).expect("解析 core Cargo.toml");

    let deps = doc
        .get("dependencies")
        .expect("core 必须有 [dependencies]")
        .as_table()
        .expect("dependencies 必须是 table");

    for name in deps.keys() {
        assert!(
            ALLOWED_DEPS.contains(&name.as_str()),
            "core 引入了未在白名单的依赖 `{name}`（架构守卫，见 modules.md §1.4/§0.2）"
        );
        assert!(
            !FORBIDDEN_PREFIXES.iter().any(|p| name.starts_with(p)),
            "core 引入了领域/重依赖 `{name}`（架构守卫，禁止逆向依赖）"
        );
    }
}

#[test]
fn core_has_no_domain_crate_optional_deps() {
    let manifest = fs::read_to_string(core_manifest()).expect("读取 core Cargo.toml");
    let doc: toml::Value = toml::from_str(&manifest).expect("解析 core Cargo.toml");

    // 检查 [features] 中是否有 `dep:minicoding-*` 引用（optional 领域依赖）。
    let features = doc
        .get("features")
        .expect("core 必须有 [features]")
        .as_table()
        .expect("features 必须是 table");

    for (feature, values) in features {
        if let Some(list) = values.as_array() {
            for v in list {
                let v = v.as_str().unwrap_or_default();
                if v.starts_with("dep:minicoding-") {
                    panic!("feature `{feature}` 引用了领域 crate 依赖 `{v}`（架构守卫，禁止）");
                }
            }
        }
    }
}
