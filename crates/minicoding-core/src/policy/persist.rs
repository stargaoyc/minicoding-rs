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

/// policy.toml 持久化错误（CORE-14：由裸 `String` 收敛为具体类型，
/// AGENTS §2.3 thiserror 约定）。
#[derive(Debug, thiserror::Error)]
pub enum PolicyPersistError {
    /// 既有文件解析失败。
    #[error("policy.toml 解析失败: {0}")]
    Parse(String),
    /// 序列化失败。
    #[error("policy.toml 序列化失败: {0}")]
    Serialize(String),
    /// 文件 IO 失败（创建目录/写 tmp/rename）。
    #[error("policy.toml 写入失败: {0}")]
    Io(#[from] std::io::Error),
}

/// 持久化策略存储（tool / `tool@path-prefix` → 决策）。
///
/// R10-09：支持 workdir 作用域——构造时传入 `workdir` 后，写入键带
/// `<workdir>:` 前缀（如 `"/proj/a:fs.write@src"`），查询仅匹配当前 workdir
/// 的键（跨项目同名路径不再误放行）。旧的无前缀键（历史数据）仍按全局规则
/// 匹配，保证向后兼容。
#[derive(Debug, Clone)]
pub struct PolicyPersist {
    path: Utf8PathBuf,
    /// 权限作用域 workdir（规范化绝对路径）。`None` 时不带作用域前缀（旧行为）。
    workdir: Option<Utf8PathBuf>,
}

/// policy.toml 磁盘结构。
#[derive(Debug, Default, Serialize, Deserialize)]
struct PolicyFile {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    allow: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    deny: BTreeMap<String, String>,
    /// R10-09：规则创建时间（键 → RFC3339）。超过 [`POLICY_TTL`] 的规则查询时
    /// 视为过期（惰性：不主动清理，命中时忽略）。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    created_at: BTreeMap<String, String>,
}

/// 权限规则 TTL（R10-09：30 天）。超过后自动失效，需重新确认。
const POLICY_TTL: time::Duration = time::Duration::days(30);

impl PolicyPersist {
    /// 创建指向 `path` 的存储（调用方经 [`crate::paths::policy_path`] 构造）。
    #[must_use]
    pub fn new(path: Utf8PathBuf) -> Self {
        Self {
            path,
            workdir: None,
        }
    }

    /// 创建带 workdir 作用域的存储（R10-09：跨项目同名路径不再误放行）。
    ///
    /// `workdir` 会 canonicalize 后作为键前缀；无法规范化时降级为原样路径。
    #[must_use]
    pub fn with_workdir(path: Utf8PathBuf, workdir: Utf8PathBuf) -> Self {
        let workdir = std::fs::canonicalize(workdir.as_std_path())
            .ok()
            .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
            .unwrap_or(workdir);
        Self {
            path,
            workdir: Some(workdir),
        }
    }

    /// 当前 workdir 作用域前缀（`"<workdir>:"`）；无 workdir 时为空串。
    fn scope_prefix(&self) -> String {
        self.workdir
            .as_ref()
            .map_or_else(String::new, |wd| format!("{wd}:"))
    }

    /// 构造查询/写入键：`{scope}{tool}` 或 `{scope}{tool}@{prefix}`。
    fn key(&self, tool: &str, prefix: Option<&str>) -> String {
        match prefix {
            Some(p) => format!("{}{tool}@{p}", self.scope_prefix()),
            None => format!("{}{tool}", self.scope_prefix()),
        }
    }

    /// 判断键是否属于当前作用域（无前缀的旧键视为全局，仍匹配）。
    fn in_scope(&self, key: &str) -> bool {
        let scope = self.scope_prefix();
        key.starts_with(&scope) || !key.contains(':')
    }

    /// 规则是否过期（`created_at` 缺失视为不过期——历史数据兼容）。
    /// 规则是否过期（`created_at` 缺失视为不过期——历史数据兼容）。
    fn expired(file: &PolicyFile, key: &str) -> bool {
        let Some(created) = file.created_at.get(key) else {
            return false;
        };
        let Ok(ts) =
            time::OffsetDateTime::parse(created, &time::format_description::well_known::Rfc3339)
        else {
            return false;
        };
        time::OffsetDateTime::now_utc() - ts > POLICY_TTL
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
        // 路径级：分别取 deny/allow 最长命中前缀长度。
        // 键形如 `tool@prefix`（旧式全局）或 `<workdir>:tool@prefix`（R10-09 作用域）；
        // 仅匹配属于当前 workdir 的键 + 旧式无 `:` 全局键；TTL 过期键跳过。
        // `longest` 定义为关联函数外提（items_after_statements 约束），
        // 闭包参数在调用处绑定。
        fn longest<'a>(
            keys: impl Iterator<Item = &'a String> + Clone,
            tool: &str,
            p: &'a str,
            in_scope: &impl Fn(&str) -> bool,
            not_expired: &impl Fn(&str) -> bool,
        ) -> Option<usize> {
            keys.filter(|k| in_scope(k) && not_expired(k))
                .filter_map(|k| {
                    // 剥离可选 workdir 前缀后，剩余形如 `tool@prefix`
                    let body = k.split_once(':').map_or(k.as_str(), |(_, rest)| rest);
                    body.strip_prefix(&format!("{tool}@"))
                })
                .filter(|prefix| {
                    p.starts_with(*prefix)
                        && (prefix.is_empty()
                            || p.len() == prefix.len()
                            || p[prefix.len()..].starts_with('/'))
                })
                .map(str::len)
                .max()
        }

        let text = std::fs::read_to_string(&self.path).ok()?;
        let file: PolicyFile = toml::from_str(&text).ok()?;

        if let Some(p) = path {
            let in_scope = |k: &str| self.in_scope(k);
            let not_expired = |k: &str| !Self::expired(&file, k);
            let deny_hit = longest(file.deny.keys(), tool, p, &in_scope, &not_expired);
            let allow_hit = longest(file.allow.keys(), tool, p, &in_scope, &not_expired);
            match (deny_hit, allow_hit) {
                (Some(d), Some(a)) => return Some(d < a),
                (Some(_), None) => return Some(false),
                (None, Some(_)) => return Some(true),
                (None, None) => {}
            }
        }

        // 工具级：deny 优先（fail-closed）；先查作用域键，再回退旧式全局键。
        let deny_key = self.key(tool, None);
        for k in [&deny_key, tool] {
            if file.deny.contains_key(k) && !Self::expired(&file, k) {
                return Some(false);
            }
        }
        let allow_key = self.key(tool, None);
        for k in [&allow_key, tool] {
            if !Self::expired(&file, k)
                && let Some(v) = file.allow.get(k)
            {
                return Some(*v);
            }
        }
        None
    }

    /// 记录工具级 allow 规则并原子落盘（unix 0600）。
    ///
    /// # Errors
    /// 读/序列化/写入失败时返回 [`PolicyPersistError`]。
    pub fn set_allow(&self, tool: &str) -> Result<(), PolicyPersistError> {
        let key = self.key(tool, None);
        self.mutate(move |f| {
            f.allow.insert(key.clone(), true);
            f.deny.remove(&key);
            f.created_at.insert(key, now_rfc3339());
        })
    }

    /// 记录工具级 deny 规则（含原因）并原子落盘。
    ///
    /// # Errors
    /// 同 [`Self::set_allow`]。
    pub fn set_deny(&self, tool: &str, reason: &str) -> Result<(), PolicyPersistError> {
        let reason = reason.to_string();
        let key = self.key(tool, None);
        self.mutate(move |f| {
            f.deny.insert(key.clone(), reason);
            f.allow.remove(&key);
            f.created_at.insert(key, now_rfc3339());
        })
    }

    /// 记录**路径级** allow 规则（`tool@前缀`），原子落盘。
    ///
    /// # Errors
    /// 同 [`Self::set_allow`]。
    pub fn set_allow_path(&self, tool: &str, prefix: &str) -> Result<(), PolicyPersistError> {
        let key = self.key(tool, Some(prefix));
        self.mutate(move |f| {
            // R4（RT4-4）：与工具级 setter 的互斥清理对齐——路径级此前只 insert
            // 不清理对方表，查询时同长度冲突 deny 恒胜（`d < a` 等长 false），
            // 用户先 DenyAlways 后改主意 AllowAlways 被静默忽略。
            f.allow.insert(key.clone(), true);
            f.deny.remove(&key);
            f.created_at.insert(key, now_rfc3339());
        })
    }

    /// 记录**路径级** deny 规则，原子落盘。
    ///
    /// # Errors
    /// 同 [`Self::set_allow`]。
    pub fn set_deny_path(
        &self,
        tool: &str,
        prefix: &str,
        reason: &str,
    ) -> Result<(), PolicyPersistError> {
        let reason = reason.to_string();
        let key = self.key(tool, Some(prefix));
        self.mutate(move |f| {
            // R4（RT4-4）：同上，写入 deny 同时清理 allow 同键。
            f.deny.insert(key.clone(), reason);
            f.allow.remove(&key);
            f.created_at.insert(key, now_rfc3339());
        })
    }

    fn mutate(&self, f: impl FnOnce(&mut PolicyFile)) -> Result<(), PolicyPersistError> {
        let mut file = match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str::<PolicyFile>(&text)
                .map_err(|e| PolicyPersistError::Parse(e.to_string()))
                .unwrap_or_default(),
            Err(_) => PolicyFile::default(),
        };
        f(&mut file);
        let bytes = toml::to_string_pretty(&file)
            .map_err(|e| PolicyPersistError::Serialize(e.to_string()))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent.as_std_path()).map_err(PolicyPersistError::Io)?;
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
                .and_then(|()| std::fs::rename(tmp.as_std_path(), self.path.as_std_path()))
                .map_err(PolicyPersistError::Io);
        if write_result.is_err() {
            // 清理残留 tmp（best effort）
            drop(std::fs::remove_file(tmp.as_std_path()));
        }
        write_result
    }
}

/// mutate 的 tmp 文件序号（SEC-10：tmp 名含 pid+序号，防并发覆盖）。
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 当前时间 RFC3339（R10-09 `created_at` 用；格式化失败回退 epoch，理论不可达）。
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

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

    /// R10-09：workdir 作用域——项目 A 的 allow 规则不适用于项目 B 同名路径。
    #[test]
    fn workdir_scoped_rules_do_not_leak_across_projects() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("policy.toml")).expect("utf8");
        // 项目 A：`/tmp/proj-a/src` 放行
        let store_a = PolicyPersist::with_workdir(path.clone(), Utf8PathBuf::from("/tmp/proj-a"));
        store_a
            .set_allow_path("fs.write", "src")
            .expect("allow in proj-a");
        assert_eq!(
            store_a.decision_for_path("fs.write", Some("src/main.rs")),
            Some(true),
            "项目 A 内命中"
        );

        // 项目 B：`/tmp/proj-b/src` 同名路径不得命中项目 A 的规则
        let store_b = PolicyPersist::with_workdir(path, Utf8PathBuf::from("/tmp/proj-b"));
        assert_eq!(
            store_b.decision_for_path("fs.write", Some("src/main.rs")),
            None,
            "跨项目同名路径不得误放行（R10-09）"
        );
    }

    /// R10-09：旧式无前缀键（历史 policy.toml）仍作为全局规则匹配（向后兼容）。
    #[test]
    fn legacy_unscoped_keys_still_match() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("policy.toml")).expect("utf8");
        let store = PolicyPersist::with_workdir(path.clone(), Utf8PathBuf::from("/tmp/proj"));
        // 手工写入旧式无前缀键（模拟历史文件）
        store.set_allow("fs.read").expect("allow");
        // set_allow 现在写作用域键；验证旧式 `fs.read` 裸键仍被识别：
        // 直接改磁盘文件模拟历史数据
        let legacy = "fs.read";
        let mut file: PolicyFile =
            toml::from_str(&std::fs::read_to_string(&path).expect("read policy")).expect("parse");
        file.allow.remove(&store.key(legacy, None));
        file.allow.insert(legacy.to_string(), true);
        std::fs::write(&path, toml::to_string_pretty(&file).expect("serialize"))
            .expect("write legacy");
        assert_eq!(
            store.decision_for("fs.read"),
            Some(true),
            "旧式无前缀键仍作为全局规则匹配"
        );
    }

    /// R10-09：TTL 过期规则惰性失效（`created_at` 超过 30 天后不再命中）。
    #[test]
    fn ttl_expired_rules_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("policy.toml")).expect("utf8");
        let store = PolicyPersist::with_workdir(path.clone(), Utf8PathBuf::from("/tmp/proj"));
        store.set_allow("fs.read").expect("allow");
        // 篡改 created_at 为 31 天前 → 过期
        let key = store.key("fs.read", None);
        let mut file: PolicyFile =
            toml::from_str(&std::fs::read_to_string(&path).expect("read policy")).expect("parse");
        let expired = (time::OffsetDateTime::now_utc() - time::Duration::days(31))
            .format(&time::format_description::well_known::Rfc3339)
            .expect("fmt");
        file.created_at.insert(key.clone(), expired);
        std::fs::write(&path, toml::to_string_pretty(&file).expect("serialize"))
            .expect("write expired");
        assert_eq!(store.decision_for("fs.read"), None, "过期规则应惰性失效");
    }
}
