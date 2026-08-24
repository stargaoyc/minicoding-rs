//! MCP project 作用域首次批准流（C-24，见 `design.md` §19.4、`api.md` §11）。
//!
//! 防"clone 即执行"攻击：含 `.minicoding/mcp.json` 的仓库首次进入时，minicoding
//! 逐个 server 弹窗询问是否启用，结果落 `~/.minicoding/mcp_choices.toml`。
//! 未批准的 project 作用域 server 不连接、不注册工具。
//!
//! 批准状态按"项目路径指纹"存储（`project_path_fingerprint`），避免不同仓库同名
//! server 互相覆盖。指纹用项目根目录的 canonical path（跨符号链接稳定）。

use std::collections::HashMap;
use std::path::Path;

use camino::Utf8PathBuf;
use minicoding_core::mcp::{McpScope, McpServerConfig};
use minicoding_core::model::McpError;
use minicoding_core::policy::{Decision, PermissionPrompt, PermissionPrompter, PromptOption, Risk};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// project 作用域 server 的批准状态（单个 server 一条记录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    /// 用户已批准启用该 server。
    Approved,
    /// 用户已拒绝启用该 server（不再弹窗，除非 `reset-project-choices`）。
    Rejected,
}

/// 单个 project 作用域 server 的批准记录。
///
/// 字段为 `pub(crate)` 仅供本模块内操作；外部消费者通过 `list_project_choices`
/// 获取 `(server, state)` 元组，不直接接触此结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    /// 项目根目录的 canonical path（人类可读，便于审计）。
    pub(crate) project_path: String,
    /// server 名称。
    pub(crate) server: String,
    /// 批准状态。
    pub(crate) state: ApprovalState,
    /// 决策时间（RFC 3339）。
    pub(crate) decided_at: String,
}

/// `mcp_choices.toml` 的根结构（按项目指纹分桶）。
///
/// 结构：`choices[<fingerprint>][<server>] = { state, project_path, decided_at }`。
/// 用 `HashMap` 便于 TOML 序列化为 table of tables。
///
/// 字段为 `pub(crate)`：`ChoicesStore` trait 把本类型作为 opaque token 暴露给
/// 调用方（load → 修改 → save），但调用方不直接读写字段，所有变更通过本模块的
/// `check_project_scope_approval`/`reset_project_choices` 完成。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ChoicesFile {
    /// 文件格式版本（未来 schema 变更时迁移用）。
    pub(crate) version: u32,
    /// 按项目指纹索引的批准表。
    pub(crate) choices: HashMap<String, HashMap<String, ApprovalRecord>>,
}

impl ChoicesFile {
    const CURRENT_VERSION: u32 = 1;

    fn new() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            choices: HashMap::new(),
        }
    }
}

/// project 作用域批准存储抽象（便于测试注入 stub）。
///
/// 生产实现为 `FileChoicesStore`（读写 `~/.minicoding/mcp_choices.toml`）。
/// 测试可用 `InMemoryChoicesStore` 避免 IO。
pub trait ChoicesStore: Send + Sync {
    /// 加载全部 choices（不存在时返回空）。
    ///
    /// # Errors
    /// 文件读取或 TOML 解析失败时返回 `McpError::Config`。
    fn load(&self) -> Result<ChoicesFile, McpError>;

    /// 保存全部 choices（原子写：先写临时文件再 rename）。
    ///
    /// # Errors
    /// 序列化、临时文件写入或 rename 失败时返回 `McpError::Config`。
    fn save(&self, choices: &ChoicesFile) -> Result<(), McpError>;
}

/// 文件系统实现：读写 `~/.minicoding/mcp_choices.toml`（0600 权限）。
pub struct FileChoicesStore {
    path: Utf8PathBuf,
}

impl FileChoicesStore {
    /// 创建指向 `path` 的存储（由 CLI 在 `paths::mcp_choices_path()` 构造）。
    #[must_use]
    pub fn new(path: Utf8PathBuf) -> Self {
        Self { path }
    }
}

impl ChoicesStore for FileChoicesStore {
    fn load(&self) -> Result<ChoicesFile, McpError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ChoicesFile::new());
            }
            Err(e) => {
                return Err(McpError::Config(format!(
                    "read mcp_choices.toml failed: {e}"
                )));
            }
        };
        let choices: ChoicesFile = toml::from_str(&text)
            .map_err(|e| McpError::Config(format!("parse mcp_choices.toml failed: {e}")))?;
        Ok(choices)
    }

    fn save(&self, choices: &ChoicesFile) -> Result<(), McpError> {
        let text = toml::to_string_pretty(choices)
            .map_err(|e| McpError::Config(format!("serialize mcp_choices.toml failed: {e}")))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| McpError::Config("mcp_choices.toml has no parent dir".into()))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| McpError::Config(format!("create mcp_choices dir failed: {e}")))?;
        // 原子写：临时文件 + rename
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .map_err(|e| McpError::Config(format!("write mcp_choices.toml.tmp failed: {e}")))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| McpError::Config(format!("rename mcp_choices.toml failed: {e}")))?;
        // 设置 0600 权限（best effort，失败不阻塞）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

/// 内存实现（测试用）。
#[cfg(test)]
#[derive(Default)]
pub struct InMemoryChoicesStore {
    inner: std::sync::Mutex<ChoicesFile>,
}

#[cfg(test)]
impl ChoicesStore for InMemoryChoicesStore {
    fn load(&self) -> Result<ChoicesFile, McpError> {
        Ok(self.inner.lock().expect("choices store poisoned").clone())
    }

    fn save(&self, choices: &ChoicesFile) -> Result<(), McpError> {
        *self.inner.lock().expect("choices store poisoned") = choices.clone();
        Ok(())
    }
}

/// 计算项目根目录的指纹（canonical path 的字符串形式）。
///
/// 用 canonical path 而非字符串原样，保证符号链接 / `..` 归一化后稳定。
/// canonicalize 失败时退回原 path（best effort，避免在不存在路径上崩溃）。
///
/// **符号链接稳定性**：canonicalize 解析所有符号链接为真实路径，使 `/symlink/to/project`
/// 与 `/real/project` 产生相同指纹。fallback 到原始字符串时，若路径含未解析的符号链接，
/// 同一项目可能因访问路径不同（`/symlink` vs `/real`）产生不同指纹，导致首次批准状态
/// 不匹配。这是 best-effort 的已知限制——canonicalize 失败通常意味着路径不存在或权限不足，
/// 此时项目尚在初始化阶段，批准状态丢失影响可控（用户重新批准即可）。
fn project_fingerprint(project_root: &Path) -> String {
    match project_root.canonicalize() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => project_root.to_string_lossy().into_owned(),
    }
}

/// 检查 project 作用域 server 的批准状态，未批准的弹窗询问（C-24）。
///
/// 流程（见 `design.md` §19.4）：
/// 1. 加载 `mcp_choices.toml`，按项目指纹查找已决策的 server；
/// 2. 已 `Approved` → 保留在结果中；已 `Rejected` → 从结果中剔除；
/// 3. 未决策 → 调 `prompter` 弹窗询问，落库后按状态保留或剔除。
///
/// `local`/`user` 作用域 server 直接保留（无需批准）。
///
/// # Errors
/// - `McpError::Config`：choices 文件读写/解析失败。
pub async fn check_project_scope_approval(
    configs: Vec<McpServerConfig>,
    project_root: &Utf8PathBuf,
    store: &dyn ChoicesStore,
    prompter: &dyn PermissionPrompter,
) -> Result<Vec<McpServerConfig>, McpError> {
    let fingerprint = project_fingerprint(project_root.as_std_path());
    let mut choices = store.load()?;
    let project_choices = choices.choices.entry(fingerprint.clone()).or_default();

    let mut result = Vec::with_capacity(configs.len());
    let mut changed = false;

    for cfg in configs {
        if cfg.scope != McpScope::Project {
            result.push(cfg);
            continue;
        }
        match project_choices.get(&cfg.name) {
            Some(rec) if rec.state == ApprovalState::Approved => {
                tracing::info!(
                    server = %cfg.name,
                    project = %fingerprint,
                    "mcp server already approved"
                );
                result.push(cfg);
            }
            Some(rec) if rec.state == ApprovalState::Rejected => {
                tracing::info!(
                    server = %cfg.name,
                    project = %fingerprint,
                    "mcp server previously rejected, skipping"
                );
            }
            _ => {
                // 未决策：弹窗询问
                let prompt = PermissionPrompt {
                    id: Ulid::new().to_string(),
                    tool: format!("mcp__{}", cfg.name),
                    summary: format!(
                        "Project MCP server `[{}]` (from .minicoding/mcp.json) requests to start.\n\
                         Transport: {}",
                        cfg.name,
                        transport_summary(&cfg)
                    ),
                    risk: Risk::Medium,
                    options: vec![PromptOption::AllowOnce, PromptOption::DenyOnce],
                };
                let decision = prompter.prompt(prompt).await;
                let state = match decision {
                    // 该 prompt 仅提供 Once 选项（无 Always 变体可达，防御性归并）
                    Decision::Allow | Decision::AllowAlways => ApprovalState::Approved,
                    Decision::Deny(_) | Decision::DenyAlways(_) => ApprovalState::Rejected,
                };
                let now = time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| "unknown".to_string());
                project_choices.insert(
                    cfg.name.clone(),
                    ApprovalRecord {
                        project_path: fingerprint.clone(),
                        server: cfg.name.clone(),
                        state,
                        decided_at: now,
                    },
                );
                changed = true;
                if state == ApprovalState::Approved {
                    result.push(cfg);
                }
            }
        }
    }

    if changed {
        store.save(&choices)?;
    }

    Ok(result)
}

/// 重置某项目指纹下的所有批准记录（`minicoding mcp reset-project-choices`）。
///
/// # Errors
/// `McpError::Config`：choices 文件读写失败。
pub fn reset_project_choices(
    project_root: &Utf8PathBuf,
    store: &dyn ChoicesStore,
) -> Result<(), McpError> {
    let fingerprint = project_fingerprint(project_root.as_std_path());
    let mut choices = store.load()?;
    if choices.choices.remove(&fingerprint).is_some() {
        store.save(&choices)?;
    }
    Ok(())
}

/// 直接设置某 project 作用域 server 的批准状态（`minicoding mcp approve <server>` 用）。
///
/// 与 `check_project_scope_approval` 的交互流不同：不弹窗，直接写入指定状态。
/// 用于 CLI `mcp approve`/`mcp reject` 子命令批量管理批准状态。
///
/// # Errors
/// `McpError::Config`：choices 文件读写失败。
pub fn set_project_approval(
    project_root: &Utf8PathBuf,
    server: &str,
    state: ApprovalState,
    store: &dyn ChoicesStore,
) -> Result<(), McpError> {
    let fingerprint = project_fingerprint(project_root.as_std_path());
    let mut choices = store.load()?;
    let project_choices = choices.choices.entry(fingerprint.clone()).or_default();
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    project_choices.insert(
        server.to_string(),
        ApprovalRecord {
            project_path: fingerprint,
            server: server.to_string(),
            state,
            decided_at: now,
        },
    );
    store.save(&choices)
}

/// 返回某项目指纹下所有 server 的批准状态（`minicoding mcp list` 用）。
///
/// # Errors
/// `McpError::Config`：choices 文件读失败。
pub fn list_project_choices(
    project_root: &Utf8PathBuf,
    store: &dyn ChoicesStore,
) -> Result<Vec<(String, ApprovalState)>, McpError> {
    let fingerprint = project_fingerprint(project_root.as_std_path());
    let choices = store.load()?;
    let Some(map) = choices.choices.get(&fingerprint) else {
        return Ok(Vec::new());
    };
    Ok(map
        .iter()
        .map(|(name, rec)| (name.clone(), rec.state))
        .collect())
}

fn transport_summary(cfg: &McpServerConfig) -> String {
    match &cfg.transport {
        minicoding_core::mcp::McpTransport::Stdio { command, args, .. } => {
            format!("stdio: {command} {}", args.join(" "))
        }
        minicoding_core::mcp::McpTransport::Http { url, .. } => {
            format!("http: {url}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minicoding_core::mcp::{McpScope, McpTransport};
    use minicoding_core::policy::PermissionPrompt;
    use std::sync::Arc;

    /// prompter stub：按预设序列返回决策。
    struct ScriptedPrompter {
        decisions: std::sync::Mutex<Vec<Decision>>,
    }

    impl ScriptedPrompter {
        fn new(decisions: Vec<Decision>) -> Self {
            Self {
                decisions: std::sync::Mutex::new(decisions),
            }
        }
    }

    impl PermissionPrompter for ScriptedPrompter {
        fn prompt(
            &self,
            _req: PermissionPrompt,
        ) -> minicoding_core::provider::BoxFuture<'_, Decision> {
            let d = self
                .decisions
                .lock()
                .expect("scripted prompter poisoned")
                .pop()
                .unwrap_or(Decision::Deny("no more scripted decisions".into()));
            Box::pin(async move { d })
        }
    }

    fn stdio_config(name: &str, scope: McpScope) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio {
                command: "echo".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
            scope,
            startup_timeout_sec: 20,
            tool_timeout_sec: 60,
            enabled: true,
            required: false,
            enabled_tools: None,
            trust_read_only_hint: false,
        }
    }

    #[tokio::test]
    async fn local_scope_no_approval_needed() {
        let store = InMemoryChoicesStore::default();
        let prompter = ScriptedPrompter::new(vec![]);
        let configs = vec![stdio_config("local-srv", McpScope::Local)];

        let result = check_project_scope_approval(
            configs,
            &Utf8PathBuf::from("/tmp/proj"),
            &store,
            &prompter,
        )
        .await
        .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "local-srv");
        // local 作用域不写 choices
        let choices = store.load().unwrap();
        assert!(
            choices.choices.is_empty(),
            "expected empty: choices.choices"
        );
    }

    #[tokio::test]
    async fn project_scope_first_time_prompts_and_approves() {
        let store = InMemoryChoicesStore::default();
        let prompter = ScriptedPrompter::new(vec![Decision::Allow]);
        let configs = vec![stdio_config("proj-srv", McpScope::Project)];

        let result = check_project_scope_approval(
            configs,
            &Utf8PathBuf::from("/tmp/proj"),
            &store,
            &prompter,
        )
        .await
        .unwrap();

        assert_eq!(result.len(), 1);
        // 决策落库
        let choices = store.load().unwrap();
        assert_eq!(choices.choices.len(), 1);
    }

    #[tokio::test]
    async fn project_scope_rejected_skipped() {
        let store = InMemoryChoicesStore::default();
        let prompter = ScriptedPrompter::new(vec![Decision::Deny("no".into())]);
        let configs = vec![stdio_config("proj-srv", McpScope::Project)];

        let result = check_project_scope_approval(
            configs,
            &Utf8PathBuf::from("/tmp/proj"),
            &store,
            &prompter,
        )
        .await
        .unwrap();

        assert!(result.is_empty(), "expected empty: result");
    }

    #[tokio::test]
    async fn project_scope_approved_persists_across_calls() {
        let store = Arc::new(InMemoryChoicesStore::default());
        let prompter = ScriptedPrompter::new(vec![Decision::Allow]);
        let configs = vec![stdio_config("proj-srv", McpScope::Project)];

        // 第一次：弹窗批准
        let result = check_project_scope_approval(
            configs.clone(),
            &Utf8PathBuf::from("/tmp/proj"),
            store.as_ref(),
            &prompter,
        )
        .await
        .unwrap();
        assert_eq!(result.len(), 1);

        // 第二次：不再弹窗（prompter 已无预设决策，会返回 Deny，但不应被调用）
        let result2 = check_project_scope_approval(
            configs,
            &Utf8PathBuf::from("/tmp/proj"),
            store.as_ref(),
            &ScriptedPrompter::new(vec![]),
        )
        .await
        .unwrap();
        assert_eq!(result2.len(), 1);
    }

    #[tokio::test]
    async fn reset_clears_choices() {
        let store = InMemoryChoicesStore::default();
        let prompter = ScriptedPrompter::new(vec![Decision::Allow]);
        let configs = vec![stdio_config("proj-srv", McpScope::Project)];
        let project = Utf8PathBuf::from("/tmp/proj");

        check_project_scope_approval(configs, &project, &store, &prompter)
            .await
            .unwrap();
        assert!(!store.load().unwrap().choices.is_empty());

        reset_project_choices(&project, &store).unwrap();
        assert!(store.load().unwrap().choices.is_empty());
    }
}
