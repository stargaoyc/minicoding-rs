//! 用户级权限决策持久化（2026-08-23 审查遗留#3，含路径粒度升级）。
//!
//! `AllowAlways`/`DenyAlways` 落盘到 `~/.minicoding/policy.toml`：
//!
//! ```toml
//! [allow]
//! "fs.write" = true
//! "fs.write@src/generated" = true
//!
//! [deny]
//! "shell.run" = "user declined network commands"
//! "fs.write@src/generated/internal" = "no internal writes"
//! ```
//!
//! 语义与边界：
//! - **两级粒度**：键为 `"tool"`（工具级）或 `"tool@相对路径前缀"`（路径级，
//!   specificity 更高）。查询取**最长命中前缀**；同长度跨表冲突时 **deny 胜**
//!   （fail-closed）；无路径命中时回退工具级，仍 deny 优先。
//! - **C-23 安全**：项目约束文件（AGENTS.md 等）的询问不提供 Always 选项，
//!   且消费方仅在 prompt 选项含 Always 时查表——受保护文件不会被持久化规则
//!   静默放行。
//! - **0600**：unix 下落盘后收紧权限（与 `mcp_choices.toml` 同标准，C-04）。

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 持久化策略存储（tool / `tool@path-prefix` → 决策）。
#[derive(Debug, Clone)]
pub struct PolicyPersist {
    path: Utf8PathBuf,
}

/// policy.toml 磁盘结构。
#[derive(Debug, Default, Serialize, Deserialize)]
struct PolicyFile {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    allow: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    deny: BTreeMap<String, String>,
}

impl PolicyPersist {
    /// 创建指向 `path` 的存储（调用方经 [`crate::paths::policy_path`] 构造）。
    #[must_use]
    pub fn new(path: Utf8PathBuf) -> Self {
        Self { path }
    }

    /// 工具级查询：`Some(true)`=allow，`Some(false)`=deny，`None`=无记录。
    ///
    /// 文件不存在/解析失败一律视为无记录（fail-open 回正常询问链，持久化层
    /// 故障不阻塞会话）。
    #[must_use]
    pub fn decision_for(&self, tool: &str) -> Option<bool> {
        self.decision_for_path(tool, None)
    }

    /// 路径感知查询（遗留#3 升级）：`path` 为工作目录内相对路径。
    ///
    /// specificity 从高到低：
    /// 1. `deny[tool@prefix]` / `allow[tool@prefix]` 中**最长命中前缀**者；
    ///    同长度跨表冲突时 deny 胜；
    /// 2. 无路径命中时回退工具级：`deny[tool]` 优先于 `allow[tool]`
    ///    （fail-closed）；均无则 `None`。
    #[must_use]
    pub fn decision_for_path(&self, tool: &str, path: Option<&str>) -> Option<bool> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let file: PolicyFile = toml::from_str(&text).ok()?;
        if let Some(p) = path {
            // 路径级：分别取 deny/allow 最长命中前缀长度（键以 `tool@` 引导）。
            // 前缀命中须落在 `/` 组件边界——裸 starts_with 会使 `src/generated`
            // 误命中兄弟目录 `src/generated-evil/x`（S18 同类 bug 的 persist 层
            // 补修，2026-08-25 审查 §6.2-S12）
            fn longest<'a, V>(
                keys: std::collections::btree_map::Keys<'a, String, V>,
                tool: &str,
                p: &'a str,
            ) -> Option<usize> {
                keys.filter_map(|k| k.strip_prefix(&format!("{tool}@")))
                    .filter(|prefix| {
                        p.starts_with(*prefix)
                            && (prefix.is_empty()
                                || p.len() == prefix.len()
                                || p[prefix.len()..].starts_with('/'))
                    })
                    .map(str::len)
                    .max()
            }
            let deny_hit = longest(file.deny.keys(), tool, p);
            let allow_hit = longest(file.allow.keys(), tool, p);
            match (deny_hit, allow_hit) {
                (Some(d), Some(a)) => return Some(d < a),
                (Some(_), None) => return Some(false),
                (None, Some(_)) => return Some(true),
                (None, None) => {}
            }
        }
        // 工具级：deny 优先（fail-closed）
        if file.deny.contains_key(tool) {
            return Some(false);
        }
        file.allow.get(tool).copied()
    }

    /// 记录工具级 allow 规则并原子落盘（unix 0600）。
    ///
    /// # Errors
    /// 读/序列化/写入失败时返回错误字符串。
    pub fn set_allow(&self, tool: &str) -> Result<(), String> {
        self.mutate(|f| {
            f.allow.insert(tool.to_string(), true);
            f.deny.remove(tool);
        })
    }

    /// 记录工具级 deny 规则（含原因）并原子落盘。
    ///
    /// # Errors
    /// 同 [`Self::set_allow`]。
    pub fn set_deny(&self, tool: &str, reason: &str) -> Result<(), String> {
        let reason = reason.to_string();
        self.mutate(move |f| {
            f.deny.insert(tool.to_string(), reason);
            f.allow.remove(tool);
        })
    }

    /// 记录**路径级** allow 规则（`tool@前缀`），原子落盘。
    ///
    /// # Errors
    /// 同 [`Self::set_allow`]。
    pub fn set_allow_path(&self, tool: &str, prefix: &str) -> Result<(), String> {
        self.mutate(|f| {
            f.allow.insert(format!("{tool}@{prefix}"), true);
        })
    }

    /// 记录**路径级** deny 规则，原子落盘。
    ///
    /// # Errors
    /// 同 [`Self::set_allow`]。
    pub fn set_deny_path(&self, tool: &str, prefix: &str, reason: &str) -> Result<(), String> {
        let reason = reason.to_string();
        self.mutate(move |f| {
            f.deny.insert(format!("{tool}@{prefix}"), reason);
        })
    }

    fn mutate(&self, f: impl FnOnce(&mut PolicyFile)) -> Result<(), String> {
        let mut file = match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str::<PolicyFile>(&text)
                .map_err(|e| format!("policy.toml 解析失败: {e}"))
                .unwrap_or_default(),
            Err(_) => PolicyFile::default(),
        };
        f(&mut file);
        let bytes =
            toml::to_string_pretty(&file).map_err(|e| format!("policy.toml 序列化失败: {e}"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent.as_std_path())
                .map_err(|e| format!("创建目录失败: {e}"))?;
        }
        // 0600 创建收敛到 util::fs_private（S7 单一事实源；unix 下 OpenOptions
        // 原子指定 mode，避免"先写后 chmod"竞态，且对已存在的宽权限文件兜底收紧）。
        // SEC-10（2026-08-25 R2 审查）：tmp 名加 pid+原子计数——固定 `{path}.tmp`
        // 在多会话并发 mutate 时互相覆盖或让 rename 发布半写文件。
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = Utf8PathBuf::from(format!(
            "{}.{}.{}.tmp",
            self.path.as_str(),
            std::process::id(),
            seq,
        ));
        let write_result =
            crate::util::fs_private::write_private(tmp.as_std_path(), bytes.as_bytes())
                .map_err(|e| format!("{e}"))
                .and_then(|()| {
                    std::fs::rename(tmp.as_std_path(), self.path.as_std_path())
                        .map_err(|e| format!("rename 失败: {e}"))
                });
        if write_result.is_err() {
            // 清理残留 tmp（best effort）
            drop(std::fs::remove_file(tmp.as_std_path()));
        }
        write_result
    }
}

/// mutate 的 tmp 文件序号（SEC-10：tmp 名含 pid+序号，防并发覆盖）。
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_deny_roundtrip_and_overwrite() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("policy.toml")).expect("utf8");
        let store = PolicyPersist::new(path);

        assert_eq!(store.decision_for("fs.write"), None);
        store.set_allow("fs.write").expect("set_allow");
        assert_eq!(store.decision_for("fs.write"), Some(true));
        store
            .set_deny("fs.write", "user declined")
            .expect("set_deny");
        assert_eq!(store.decision_for("fs.write"), Some(false));
        store.set_allow("shell.run").expect("allow shell");
        assert_eq!(store.decision_for("shell.run"), Some(true));
        assert_eq!(store.decision_for("fs.write"), Some(false));
    }

    #[test]
    fn missing_file_is_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("none.toml")).expect("utf8");
        let store = PolicyPersist::new(path);
        assert_eq!(store.decision_for("any.tool"), None);
    }

    #[test]
    fn path_specificity_and_deny_priority() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p.toml")).expect("utf8");
        let store = PolicyPersist::new(path);
        store.set_allow("fs.write").expect("allow tool");
        store
            .set_allow_path("fs.write", "src/generated")
            .expect("allow path");
        store
            .set_deny_path("fs.write", "src/generated/internal", "no internal writes")
            .expect("deny path");

        // 工具级 allow 兜底
        assert_eq!(store.decision_for_path("fs.write", None), Some(true));
        assert_eq!(
            store.decision_for_path("fs.write", Some("src/main.rs")),
            Some(true)
        );
        // 命中 allow 路径前缀
        assert_eq!(
            store.decision_for_path("fs.write", Some("src/generated/a.rs")),
            Some(true)
        );
        // 更长 deny 前缀胜过较短 allow 前缀
        assert_eq!(
            store.decision_for_path("fs.write", Some("src/generated/internal/x.rs")),
            Some(false)
        );
        // deny 路径独立于 allow 表存在时同样生效
        assert_eq!(
            store.decision_for_path("shell.run", None),
            None,
            "未配置的工具不受影响"
        );
    }

    #[test]
    fn path_prefix_requires_component_boundary() {
        // 2026-08-25 审查 S-12：裸 starts_with 会使 `src/generated` 命中
        // 兄弟目录 `src/generated-evil/...`，前缀匹配须落在 `/` 组件边界
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("p.toml")).expect("utf8");
        let store = PolicyPersist::new(path);
        store
            .set_allow_path("fs.write", "src/generated")
            .expect("allow path");

        assert_eq!(
            store.decision_for_path("fs.write", Some("src/generated-evil/x")),
            None,
            "兄弟目录前缀碰撞不得命中"
        );
        assert_eq!(
            store.decision_for_path("fs.write", Some("src/generated/x")),
            Some(true),
            "组件边界内的子路径正常命中"
        );
        assert_eq!(
            store.decision_for_path("fs.write", Some("src/generated")),
            Some(true),
            "与前缀完全相等的路径命中"
        );
    }
}
