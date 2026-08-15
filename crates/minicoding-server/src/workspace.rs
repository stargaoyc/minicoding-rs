//! Workspace 端点（W-11 项目工作区，见 `design.md` §26.9）。
//!
//! 只读浏览（root/list/read）等价 `fs.read`（C-01 仅约束副作用），不经
//! `PermissionPolicy`，但记录审计日志（只读浏览轨迹，供会话审计）；路径一律
//! 经 `resolve_path` 相对 workdir 解析 + C-03 越界校验。切换工作区
//! （`POST /workspace`）走 `Runtime::switch_workdir`（Ask 审批 + 审计 + SSE
//! 权限弹窗，复用 W-03 前端机制）。

use crate::http::{AppState, HttpError};
use crate::session_mgr::SessionManagerError;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use minicoding_core::storage::{AuditKind, AuditRecord};
use minicoding_protocol::workspace::{
    WorkspaceDiffEntry, WorkspaceDiffResponse, WorkspaceFileChange, WorkspaceListEntry,
    WorkspaceListResponse, WorkspaceReadResponse, WorkspaceRoot, WorkspaceSwitchResponse,
};
use minicoding_tools::resolve_path;
use serde::Deserialize;
use time::OffsetDateTime;

/// 前端忽略目录（大目录/构建产物，避免文件树拉取无意义内容，C-07 资源约束）。
const IGNORE_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    ".next",
    ".nuxt",
    "coverage",
    "vendor",
    "venv",
    ".venv",
    "__pycache__",
    ".cache",
    ".minicoding",
];

/// 文件内容读取上限（C-07：与 `fs.read` 对齐，超出截断）。
const MAX_READ_BYTES: usize = 64 * 1024;

/// `GET /workspace/list` 查询参数。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// 相对 workdir 的目录路径（省略 = 根目录）。
    #[serde(default)]
    pub path: Option<String>,
}

/// `GET /workspace/read` 查询参数。
#[derive(Debug, Deserialize)]
pub struct ReadQuery {
    /// 相对 workdir 的文件路径。
    pub path: String,
}

/// `POST /workspace` 请求 body。
#[derive(Debug, Deserialize)]
pub struct SwitchBody {
    /// 目标工作区绝对路径。
    pub path: String,
}

/// `GET /workspace/root` — 返回会话工作目录。
///
/// # Errors
/// 会话不存在时返回 404。
pub async fn workspace_root(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<WorkspaceRoot>, HttpError> {
    let session = state
        .mgr
        .get_or_load(&session_id)
        .await
        .map_err(|_| SessionManagerError::NotFound(session_id.clone()))?;
    let path = session.runtime.workdir().await;
    let name = path
        .file_name()
        .map_or_else(|| path.as_str().to_string(), str::to_string);
    Ok(Json(WorkspaceRoot {
        path: path.to_string(),
        name,
    }))
}

/// `GET /workspace/list` — 目录列表（单层，含 ignore 过滤）。
///
/// # Errors
/// 会话不存在返回 404；路径越界返回 403（C-03）；目录不存在返回 404。
pub async fn workspace_list(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<WorkspaceListResponse>, HttpError> {
    let session = state
        .mgr
        .get_or_load(&session_id)
        .await
        .map_err(|_| SessionManagerError::NotFound(session_id.clone()))?;
    let workdir = session.runtime.workdir().await;
    let dir = match &query.path {
        Some(rel) => resolve_path(&workdir, rel).map_err(|e| http_err_from_tool(&e))?,
        None => workdir.clone(),
    };

    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(&dir)
        .await
        .map_err(|e| io_err(&dir, &e))?;
    while let Some(entry) = read_dir.next_entry().await.map_err(|e| io_err_any(&e))? {
        let name = entry.file_name().to_string_lossy().to_string();
        // 隐藏文件/目录默认不展示（`list` 是浏览视图，非工具沙箱逃逸面）
        if name.starts_with('.') && name != ".minicoding" {
            continue;
        }
        let file_type = entry.file_type().await.map_err(|e| io_err_any(&e))?;
        if file_type.is_dir() {
            if IGNORE_DIRS.contains(&name.as_str()) {
                continue;
            }
            entries.push(WorkspaceListEntry {
                name,
                kind: "dir".to_string(),
                size: None,
            });
        } else if file_type.is_file() {
            let size = entry.metadata().await.ok().map(|m| m.len());
            entries.push(WorkspaceListEntry {
                name,
                kind: "file".to_string(),
                size,
            });
        }
        // 符号链接等其它类型跳过（避免循环/越界浏览）
    }
    entries.sort_by(|a, b| b.kind.cmp(&a.kind).then_with(|| a.name.cmp(&b.name)));

    record_audit(&session.runtime, "workspace.list", dir.as_ref()).await;
    Ok(Json(WorkspaceListResponse {
        path: dir.to_string(),
        entries,
    }))
}

/// `GET /workspace/read` — 文件内容（≤ 64 KiB，超出截断，C-07）。
///
/// # Errors
/// 会话不存在返回 404；路径越界返回 403（C-03）；文件不存在返回 404；
/// 目标是目录返回 400。
pub async fn workspace_read(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<ReadQuery>,
) -> Result<Json<WorkspaceReadResponse>, HttpError> {
    let session = state
        .mgr
        .get_or_load(&session_id)
        .await
        .map_err(|_| SessionManagerError::NotFound(session_id.clone()))?;
    let workdir = session.runtime.workdir().await;
    let path = resolve_path(&workdir, &query.path).map_err(|e| http_err_from_tool(&e))?;

    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|e| io_err(&path, &e))?;
    if metadata.is_dir() {
        return Err(HttpError {
            status: StatusCode::BAD_REQUEST,
            message: format!("is a directory: {}", path.as_str()),
        });
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| io_err(&path, &e))?;
    let size = bytes.len();
    let truncated = size > MAX_READ_BYTES;
    let content = String::from_utf8_lossy(&bytes[..size.min(MAX_READ_BYTES)]).to_string();

    record_audit(&session.runtime, "workspace.read", path.as_ref()).await;
    Ok(Json(WorkspaceReadResponse {
        content,
        size: size as u64,
        truncated,
    }))
}

/// `GET /workspace/diff` — 会话内文件改动历史（源自 `FileChangeJournal`）。
///
/// # Errors
/// 会话不存在返回 404；journal 未启用返回 501。
pub async fn workspace_diff(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<WorkspaceDiffResponse>, HttpError> {
    let session = state
        .mgr
        .get_or_load(&session_id)
        .await
        .map_err(|_| SessionManagerError::NotFound(session_id.clone()))?;
    let runtime = session.runtime.clone();
    let journal = runtime.journal().ok_or_else(|| HttpError {
        status: StatusCode::NOT_IMPLEMENTED,
        message: "journal 未启用".to_string(),
    })?;
    let entries = journal.diff().await.map_err(|e| HttpError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("journal diff failed: {e}"),
    })?;

    let mapped = entries
        .into_iter()
        .map(|entry| WorkspaceDiffEntry {
            op_id: entry.op_id.clone(),
            prompt_snippet: entry.prompt_snippet,
            files: entry
                .files
                .into_iter()
                .map(|f| to_dto(&f))
                .collect::<Vec<_>>(),
        })
        .collect::<Vec<_>>();
    Ok(Json(WorkspaceDiffResponse { entries: mapped }))
}

/// `POST /workspace` — 切换工作目录（Ask 审批 + 审计 + SSE 权限弹窗）。
///
/// 目标路径校验（绝对路径 + 目录存在性）在 `Runtime::switch_workdir` 内完成。
///
/// # Errors
/// 会话不存在返回 404；目标路径非法（非绝对/不存在/非目录）返回 400；
/// turn 进行中等待超过 60s 返回 409。
pub async fn workspace_switch(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<SwitchBody>,
) -> Result<Json<WorkspaceSwitchResponse>, HttpError> {
    let session = state
        .mgr
        .get_or_load(&session_id)
        .await
        .map_err(|_| SessionManagerError::NotFound(session_id.clone()))?;

    // turn 串行锁：切换与进行中的 turn 互斥（C-31 上下文一致性）。等待设
    // 60s 上限——turn 最长 600s，若先前消息仍在跑（LLM 卡住等），切换应立即
    // 返回 409 而不是无界排队（否则前端"切换中"无限转圈）。
    let _turn_guard =
        tokio::time::timeout(std::time::Duration::from_secs(60), session.turn_lock.lock())
            .await
            .map_err(|_| HttpError {
                status: StatusCode::CONFLICT,
                message: "会话忙：上一轮消息仍在处理中，请稍后再试".to_string(),
            })?;
    let target = camino::Utf8PathBuf::from(body.path);
    let switched = session
        .runtime
        .switch_workdir(&target)
        .await
        .map_err(|e| HttpError {
            status: StatusCode::BAD_REQUEST,
            message: e.to_string(),
        })?;
    let path = session.runtime.workdir().await;
    Ok(Json(WorkspaceSwitchResponse {
        switched,
        path: path.to_string(),
    }))
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// 只读浏览审计（不落权限决策，仅记录轨迹，供会话审计）。
async fn record_audit(runtime: &minicoding_core::runtime::Runtime, tool: &str, path: &str) {
    let rec = AuditRecord {
        ts: OffsetDateTime::now_utc(),
        session: runtime.session().id.clone(),
        kind: AuditKind::ToolCall,
        tool: Some(tool.to_string()),
        decision: None,
        detail: format!("workspace browse: {path}"),
    };
    if let Err(e) = runtime.audit().record(rec).await {
        tracing::warn!(error = %e, "workspace audit record failed");
    }
}

/// 把 `FileChange` 转为前端 DTO（tag 序列化，`minicoding_protocol::workspace`）。
fn to_dto(change: &minicoding_core::journal::FileChange) -> WorkspaceFileChange {
    use minicoding_core::journal::FileChange;
    let text = |b: &[u8]| String::from_utf8_lossy(b).to_string();
    match change {
        FileChange::Written {
            path,
            before,
            after,
        } => WorkspaceFileChange::Written {
            path: path.to_string(),
            before: before.as_ref().map(|b| text(b)),
            after: text(after),
        },
        FileChange::Edited {
            path,
            before,
            after,
        } => WorkspaceFileChange::Edited {
            path: path.to_string(),
            before: text(before),
            after: text(after),
        },
        FileChange::Deleted { path, content } => WorkspaceFileChange::Deleted {
            path: path.to_string(),
            content: text(content),
        },
        FileChange::Created { path, content } => WorkspaceFileChange::Created {
            path: path.to_string(),
            content: text(content),
        },
    }
}

/// `ToolError` → `HttpError`（越界映射 403，C-03 语义暴露给前端）。
fn http_err_from_tool(e: &minicoding_core::model::ToolError) -> HttpError {
    use minicoding_core::model::ToolError;
    let status = match e {
        ToolError::PathEscaped(_) => StatusCode::FORBIDDEN,
        ToolError::NotFound(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::BAD_REQUEST,
    };
    HttpError {
        status,
        message: e.to_string(),
    }
}

fn io_err(path: &camino::Utf8Path, e: &std::io::Error) -> HttpError {
    let status = if e.kind() == std::io::ErrorKind::NotFound {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    HttpError {
        status,
        message: format!("{}: {e}", path.as_str()),
    }
}

fn io_err_any(e: &std::io::Error) -> HttpError {
    HttpError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;
    use axum::http::StatusCode;
    use camino::Utf8PathBuf;
    use minicoding_core::journal::FileChange;

    #[test]
    fn file_change_to_dto_roundtrip() {
        let changes = vec![
            FileChange::Written {
                path: Utf8PathBuf::from("a.txt"),
                before: None,
                after: b"hello".to_vec(),
            },
            FileChange::Edited {
                path: Utf8PathBuf::from("b.txt"),
                before: b"old".to_vec(),
                after: b"new".to_vec(),
            },
            FileChange::Deleted {
                path: Utf8PathBuf::from("c.txt"),
                content: b"gone".to_vec(),
            },
            FileChange::Created {
                path: Utf8PathBuf::from("d.txt"),
                content: b"fresh".to_vec(),
            },
        ];
        for c in &changes {
            let dto = to_dto(c);
            let json = serde_json::to_value(&dto).expect("dto serializable");
            // tag 序列化：kind 字段区分四种变体
            assert!(json.get("kind").is_some(), "dto must carry kind tag");
            assert!(json.get("path").is_some(), "dto must carry path");
        }
        // 具体形态校验（前端依赖 tag 字段）
        let written = to_dto(&changes[0]);
        assert!(
            serde_json::to_value(&written)
                .unwrap()
                .get("kind")
                .unwrap()
                .is_string()
        );
    }

    #[test]
    fn path_escape_maps_to_forbidden() {
        let e = minicoding_core::model::ToolError::PathEscaped("../escape".to_string());
        let http = http_err_from_tool(&e);
        assert_eq!(http.status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn not_found_maps_to_404() {
        let e = minicoding_core::model::ToolError::NotFound("missing".to_string());
        let http = http_err_from_tool(&e);
        assert_eq!(http.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn io_not_found_maps_to_404() {
        let http = io_err(
            &Utf8PathBuf::from("/tmp/nope.txt"),
            &std::io::Error::new(std::io::ErrorKind::NotFound, "nope"),
        );
        assert_eq!(http.status, StatusCode::NOT_FOUND);
    }
}
