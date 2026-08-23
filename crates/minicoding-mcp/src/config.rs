//! MCP server 配置加载（user/local + project 三作用域，T-M4-10）。
//!
//! 从 CLI 迁入（2026-08-23 审查 §7-P0 接线）：sdk builder 启动序列需要同一
//! 份 `load_all_configs`，单一来源放本 crate；`minicoding mcp list` 复用。
//!
//! 文件布局：
//! - `~/.minicoding/mcp.json`：`{"local": {...}, "user": {...}}`；
//! - `<project>/.minicoding/mcp.json`：`{"servers": {...}}`（入版本控制）。

use minicoding_core::mcp::{McpScope, McpServerConfig};
use minicoding_core::paths;
use serde::Deserialize;

/// 加载所有作用域的 MCP server 配置（user + local + project）。
///
/// # Errors
/// 配置文件存在但读取/解析失败时返回 IO/反序列化错误字符串（包装为
/// `McpError::Config`）；文件不存在视为空配置。
pub fn load_all_configs(
    project_root: &camino::Utf8PathBuf,
) -> Result<Vec<McpServerConfig>, minicoding_core::model::McpError> {
    let mut configs = Vec::new();

    // user/local：~/.minicoding/mcp.json
    if let Ok(user_mcp_path) = paths::minicoding_home() {
        let user_mcp_file = user_mcp_path.join("mcp.json");
        if user_mcp_file.exists() {
            let text = std::fs::read_to_string(&user_mcp_file).map_err(|e| {
                minicoding_core::model::McpError::Config(format!("读取 {user_mcp_file} 失败: {e}"))
            })?;
            let file: UserMcpFile = serde_json::from_str(&text).map_err(|e| {
                minicoding_core::model::McpError::Config(format!("解析 {user_mcp_file} 失败: {e}"))
            })?;
            for mut cfg in file.local.servers {
                cfg.scope = McpScope::Local;
                configs.push(cfg);
            }
            for mut cfg in file.user.servers {
                cfg.scope = McpScope::User;
                configs.push(cfg);
            }
        }
    }

    // project：.minicoding/mcp.json
    let project_mcp_file = project_root.join(".minicoding").join("mcp.json");
    if project_mcp_file.exists() {
        let text = std::fs::read_to_string(&project_mcp_file).map_err(|e| {
            minicoding_core::model::McpError::Config(format!("读取 {project_mcp_file} 失败: {e}"))
        })?;
        let file: ProjectMcpFile = serde_json::from_str(&text).map_err(|e| {
            minicoding_core::model::McpError::Config(format!("解析 {project_mcp_file} 失败: {e}"))
        })?;
        for mut cfg in file.servers.servers {
            cfg.scope = McpScope::Project;
            configs.push(cfg);
        }
    }

    Ok(configs)
}

/// user/local 作用域配置文件结构。
#[derive(Deserialize)]
struct UserMcpFile {
    #[serde(default)]
    local: ScopeServers,
    #[serde(default)]
    user: ScopeServers,
}

/// project 作用域配置文件结构。
#[derive(Deserialize)]
struct ProjectMcpFile {
    #[serde(default)]
    servers: ScopeServers,
}

#[derive(Default, Deserialize)]
struct ScopeServers {
    #[serde(default)]
    servers: Vec<McpServerConfig>,
}
