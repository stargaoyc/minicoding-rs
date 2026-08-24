//! 用户级权限决策持久化（2026-08-23 审查遗留#3）。
//!
//! `AllowAlways`/`DenyAlways` 落盘到 `~/.minicoding/policy.toml`：
//!
//! ```toml
//! [allow]
//! "fs.write" = true
//!
//! [deny]
//! "shell.run" = "user declined network commands"
//! ```
//!
//! 语义与边界：
//! - **工具名粒度**（v1）：按 tool 名记忆，不含参数维度；
//! - **C-23 安全**：项目约束文件（AGENTS.md 等）的询问本就不提供 Always 选项，
//!   且消费方仅在 prompt 选项含 Always 时查表——受保护文件不会被持久化规则
//!   静默放行；
//! - **0600**：unix 下落盘后收紧权限（与 `mcp_choices.toml` 同标准，C-04）。

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 持久化策略存储（tool → 决策）。
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

    /// 查询工具的持久化决策：`Some(true)`=allow，`Some(false)`=deny，
    /// `None`=无记录。文件不存在/解析失败一律视为无记录（fail-open 回正常
    /// 询问链，持久化层故障不阻塞会话）。
    #[must_use]
    pub fn decision_for(&self, tool: &str) -> Option<bool> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let file: PolicyFile = toml::from_str(&text).ok()?;
        if file.allow.get(tool).copied().unwrap_or(false) {
            return Some(true);
        }
        if file.deny.contains_key(tool) {
            return Some(false);
        }
        None
    }

    /// 记录 allow 规则并原子落盘（unix 0600）。
    ///
    /// # Errors
    /// 读/序列化/写入失败时返回错误字符串。
    pub fn set_allow(&self, tool: &str) -> Result<(), String> {
        self.mutate(|f| {
            f.allow.insert(tool.to_string(), true);
            f.deny.remove(tool);
        })
    }

    /// 记录 deny 规则（含原因）并原子落盘。
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

    fn mutate(&self, f: impl FnOnce(&mut PolicyFile)) -> Result<(), String> {
        use std::io::Write as _;
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
        let tmp = Utf8PathBuf::from(format!("{}.tmp", self.path.as_str()));
        let mut fh = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp.as_std_path())
            .map_err(|e| format!("写临时文件失败: {e}"))?;
        fh.write_all(bytes.as_bytes()).map_err(|e| format!("{e}"))?;
        // unix：临时文件即目标权限（rename 继承）；其他平台依赖 umask
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(tmp.as_std_path(), std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(tmp.as_std_path(), self.path.as_std_path())
            .map_err(|e| format!("rename 失败: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_deny_roundtrip_and_overwrite() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(tmp.path().join("policy.toml")).expect("utf8");
        let store = PolicyPersist::new(path.clone());

        // 无记录
        assert_eq!(store.decision_for("fs.write"), None);

        // set_allow → Some(true)
        store.set_allow("fs.write").expect("set_allow");
        assert_eq!(store.decision_for("fs.write"), Some(true));

        // set_deny 覆盖 allow → Some(false)
        store
            .set_deny("fs.write", "user declined")
            .expect("set_deny");
        assert_eq!(store.decision_for("fs.write"), Some(false));

        // 再次 allow 覆盖回 true
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
}
