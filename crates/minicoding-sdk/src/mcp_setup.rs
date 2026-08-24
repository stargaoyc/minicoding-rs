//! MCP client 接线（2026-08-23 审查 §7-P0）：加载三作用域配置 → C-24 project
//! 首次批准 → 启动进程池 → 包装 `McpToolWrapper` 注册进 Runtime。
//!
//! 设计要点：
//! - **异步入口**：`build_runtime` 是同步组装（TUI 跑在 `current_thread` runtime，
//!   不能 `block_in_place`），故拆为 build 后调用的 `attach_mcp_tools(&mut rt)`——
//!   复用 `Runtime::register_dynamic_tool` 既有扩展点（与 plan.exit/task.spawn 同构）；
//! - **best-effort**：配置缺失/解析失败/启动失败仅 warn 跳过，不阻塞会话启动；
//! - **C-24**：project 作用域逐 server 弹窗（复用权限 prompter），批准落
//!   `~/.minicoding/mcp_choices.toml`（0600）；
//! - **S13/C-25**：`trust_read_only_hint` 固定 false（hint 是远端自我声明，
//!   默认按 Command 处理走完整权限链）。

use std::sync::Arc;

use camino::Utf8PathBuf;
use minicoding_core::mcp::{McpClient, ToolHint};
use minicoding_core::paths;
use minicoding_core::policy::PermissionPrompter;
use minicoding_core::runtime::Runtime;
use minicoding_mcp::client::rmcp::RmcpClient;
use minicoding_mcp::client::wrapper::McpToolWrapper;
use tracing::warn;

/// 加载配置 → 批准 → 启动 → 注册 MCP 工具到 `rt`（best-effort，见模块文档）。
///
/// # Errors
/// 仅透传 `register_dynamic_tool` 的错误（理论不失败）；MCP 自身故障均降级为 warn。
pub async fn attach_mcp_tools(
    rt: &mut Runtime,
    workdir: &Utf8PathBuf,
    prompter: Arc<dyn PermissionPrompter>,
) -> anyhow::Result<()> {
    let configs = match minicoding_mcp::config::load_all_configs(workdir) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "mcp: 配置加载失败，跳过 MCP 工具注册");
            return Ok(());
        }
    };
    if configs.is_empty() {
        return Ok(());
    }

    // C-24：project 作用域首次批准（未决策逐个弹窗；Rejected 不再弹）
    let approved = match paths::mcp_choices_path() {
        Ok(path) => {
            let store = minicoding_mcp::approval::FileChoicesStore::new(path);
            match minicoding_mcp::approval::check_project_scope_approval(
                configs,
                workdir,
                &store,
                prompter.as_ref(),
            )
            .await
            {
                Ok(a) => a,
                Err(e) => {
                    warn!(error = %e, "mcp: 批准流程失败，跳过 MCP 工具注册");
                    return Ok(());
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "mcp: 无法确定 mcp_choices.toml 路径，跳过");
            return Ok(());
        }
    };
    if approved.is_empty() {
        return Ok(());
    }

    let client: Arc<dyn McpClient> = Arc::new(RmcpClient::new());
    if let Err(e) = client.start(&approved).await {
        warn!(error = %e, "mcp: server 启动失败（required 未满足），跳过工具注册");
        return Ok(());
    }

    // 健康监督（遗留#5）：周期 health_check，死亡连接经 restart 全量重建
    //（一次性语义，避免风暴）。detach 后台 task 随 runtime 存活。
    {
        let supervisor_client = Arc::clone(&client);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                if !matches!(supervisor_client.health_check().await, Ok(true)) {
                    warn!("mcp supervisor: 检测到不健康连接，尝试 restart");
                    if let Err(e) = supervisor_client.restart().await {
                        warn!(error = %e, "mcp supervisor restart 失败");
                    }
                }
            }
        });
    }

    let hints = client.tool_hints().await;
    let mut registered = 0usize;
    for schema in client.list_tools().await {
        let Some((server, tool)) = minicoding_mcp::naming::parse_mcp_tool_name(&schema.name) else {
            continue;
        };
        let hint = hints
            .get(&schema.name)
            .copied()
            .unwrap_or(ToolHint::Unknown);
        // S13/C-25：trust_read_only_hint 固定 false（自我声明默认不信任）
        let wrapper = McpToolWrapper::new(
            Arc::clone(&client),
            server.to_string(),
            tool.to_string(),
            schema,
            hint,
            false,
        );
        rt.register_dynamic_tool(Arc::new(wrapper));
        registered += 1;
    }
    if registered > 0 {
        tracing::info!(
            servers = approved.len(),
            tools = registered,
            "mcp: 工具已注册"
        );
    }
    Ok(())
}
