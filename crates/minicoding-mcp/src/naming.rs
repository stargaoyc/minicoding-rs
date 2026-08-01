//! MCP 工具命名与解析（见 `design.md` §19.3、`api.md` §2.7）。
//!
//! 命名规约：`mcp__<server>__<tool>`，使 MCP 工具与内置工具在权限规则中可区分，
//! 且支持 `mcp__github__*` 通配（通配匹配由 `minicoding-policy` 的 `globset` 完成，
//! 此处仅负责命名与解析）。
//!
//! 安全约束（`rules.md` C-09）：工具名必须已注册才能被 `ToolRegistry` dispatch；
//! MCP 工具注册时统一用 `mcp_tool_name` 生成名字，避免 LLM 伪造未注册工具名。

use minicoding_core::model::ToolError;

/// MCP 工具名前缀（`mcp__`，与内置工具命名空间隔离）。
pub const MCP_PREFIX: &str = "mcp__";

/// 生成 MCP 工具名：`mcp__<server>__<tool>`（见 `design.md` §19.3）。
///
/// `server` 与 `tool` 不允许含 `__`（避免解析歧义），调用方应保证传入合法标识符。
/// 若含 `__` 则返回 `Err`，由 `RmcpClient::start` 早期拒绝注册。
///
/// # Errors
/// - `server` 或 `tool` 为空字符串；
/// - `server` 或 `tool` 含 `__`（会导致 `parse_mcp_tool_name` 解析歧义）。
pub fn mcp_tool_name(server: &str, tool: &str) -> Result<String, ToolError> {
    if server.is_empty() || tool.is_empty() {
        return Err(ToolError::InvalidInput(format!(
            "mcp server/tool name must not be empty: server={server:?} tool={tool:?}"
        )));
    }
    if server.contains("__") || tool.contains("__") {
        return Err(ToolError::InvalidInput(format!(
            "mcp server/tool name must not contain '__': server={server:?} tool={tool:?}"
        )));
    }
    Ok(format!("{MCP_PREFIX}{server}__{tool}"))
}

/// 解析 `mcp__<server>__<tool>` 名字回 `(server, tool)`。
///
/// 返回 `None` 表示不是合法 MCP 工具名（要么不带 `mcp__` 前缀，要么切分后不恰好
/// 是 `server__tool` 两段）。用于 `Runtime` 在 `execute_tool_calls` 中识别 MCP 工具
/// 调用并路由到 `McpClient::call`。
#[must_use]
pub fn parse_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(MCP_PREFIX)?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    // 防止 `mcp__a__b__c` 这种被误解析为 `(a, b__c)`：拒绝 server 段再含 `__`
    // （split_once 已保证 tool 段可含 `__`，但 server 段不允许）。
    if server.contains("__") {
        return None;
    }
    Some((server, tool))
}

/// 判断工具名是否为 MCP 工具（带 `mcp__` 前缀）。
#[must_use]
pub fn is_mcp_tool(name: &str) -> bool {
    name.starts_with(MCP_PREFIX) && parse_mcp_tool_name(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_generation_round_trip() {
        let name = mcp_tool_name("github", "list_prs").unwrap();
        assert_eq!(name, "mcp__github__list_prs");
        assert_eq!(parse_mcp_tool_name(&name), Some(("github", "list_prs")));
        assert!(is_mcp_tool(&name));
    }

    #[test]
    fn rejects_empty_names() {
        assert!(mcp_tool_name("", "tool").is_err());
        assert!(mcp_tool_name("server", "").is_err());
    }

    #[test]
    fn rejects_double_underscore_in_components() {
        // server 含 `__` 会让解析歧义，早期拒绝
        assert!(mcp_tool_name("a__b", "tool").is_err());
        assert!(mcp_tool_name("server", "a__b").is_err());
    }

    #[test]
    fn parse_rejects_non_mcp_names() {
        assert_eq!(parse_mcp_tool_name("fs.read"), None);
        assert_eq!(parse_mcp_tool_name("mcp__"), None);
        assert_eq!(parse_mcp_tool_name("mcp__github"), None);
        // `mcp____github__tool`（4 个下划线）：strip `mcp__` 后剩 `__github__tool`，
        // splitn 得 `("", "github__tool")` → server 段空 → 拒绝。
        assert_eq!(parse_mcp_tool_name("mcp____github__tool"), None);
        // `mcp__a__`（tool 段空）：splitn 得 `("a", "")` → tool 段空 → 拒绝。
        assert_eq!(parse_mcp_tool_name("mcp__a__"), None);
    }

    #[test]
    fn parse_allows_underscore_in_tool_name() {
        // tool 段可含单下划线（如 `list_prs`）
        let name = mcp_tool_name("github", "list_open_prs").unwrap();
        assert_eq!(
            parse_mcp_tool_name(&name),
            Some(("github", "list_open_prs"))
        );
    }

    #[test]
    fn is_mcp_tool_distinguishes_builtin() {
        assert!(!is_mcp_tool("fs.read"));
        assert!(!is_mcp_tool("shell.run"));
        assert!(is_mcp_tool("mcp__github__list_prs"));
    }
}
