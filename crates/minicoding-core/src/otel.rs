//! OpenTelemetry span 辅助与采样配置（T-M0-4）。
//!
//! 设计意图（见 `docs/design.md` §15）：
//! - **业务代码只写 `tracing` 宏**，由 subscriber 层桥接到 OTLP 或本地 fmt；
//! - **core 保持轻量**：本模块只提供 span 名/属性常量与采样配置类型；
//!   subscriber 安装（需 `tracing-subscriber`）与 OTLP 导出（需 `reqwest`）在
//!   `minicoding-cli` 中完成，避免 core 引入网络/订阅器重依赖（AGENTS.md §3.5、
//!   `modules.md` §18.6）。
//! - **降级安全**：未配置 OTLP 端点时降级为纯本地 fmt 日志，不报错（M0 验收标准）。
//!
//! 资源属性（§15.3）：`service.name = minicoding`、`service.version`、`host.name`。
//! 采样策略：`AlwaysOn`（默认，本地调试）/ `TraceIdRatio 0.1`（生产，由
//! `OTEL_TRACES_SAMPLER=traceidratio` 控制）。

/// `service.name` 资源属性值。
pub const SERVICE_NAME: &str = "minicoding";

/// Span 名约定（见 `design.md` §15.1、`docs/observability.md` §4.1）。
pub mod span_name {
    /// 会话级 span（会话开始到结束）。
    pub const SESSION: &str = "session";
    /// 单轮 span（用户输入到本轮 `EndTurn`）。
    pub const TURN: &str = "turn";
    /// LLM 流式调用 span。
    pub const LLM_CHAT_STREAM: &str = "llm.chat_stream";
    /// 工具调用 span。
    pub const TOOL_CALL: &str = "tool.call";
    /// 权限决策 span。
    pub const PERMISSION_CHECK: &str = "permission.check";
    /// 压缩 span。
    pub const COMPRESS: &str = "compress";
    // === 以下为可观测性补充（见 docs/observability.md §4.1） ===
    /// 记忆加载 span。
    pub const MEMORY_LOAD: &str = "memory.load";
    /// 记忆保存 span。
    pub const MEMORY_SAVE: &str = "memory.save";
    /// Journal 记录 span。
    pub const JOURNAL_RECORD: &str = "journal.record";
    /// Journal 回滚 span。
    pub const JOURNAL_UNDO: &str = "journal.undo";
    /// 存储追加 span。
    pub const STORAGE_APPEND: &str = "storage.append";
    /// 审计记录 span。
    pub const AUDIT_RECORD: &str = "audit.record";
    /// 沙箱注入 span。
    pub const SANDBOX_APPLY: &str = "sandbox.apply";
    /// MCP 工具调用 span。
    pub const MCP_CALL: &str = "mcp.call";
    /// MCP 连接 span。
    pub const MCP_CONNECT: &str = "mcp.connect";
    /// 上下文构建 span。
    pub const CONTEXT_BUILD: &str = "context.build";
    /// Provider 重试 span。
    pub const PROVIDER_RETRY: &str = "provider.retry";
    /// 配置热重载 span。
    pub const CONFIG_RELOAD: &str = "config.reload";
    /// 事件广播 span。
    pub const EVENT_PUBLISH: &str = "event.publish";
    /// 后台 Shell spawn span。
    pub const SHELL_BG_SPAWN: &str = "shell.bg_spawn";
}

/// Span 属性键约定（见 `design.md` §15.2）。
pub mod attr {
    pub const SESSION_ID: &str = "session.id";
    pub const SESSION_WORKDIR: &str = "session.workdir";
    pub const PROVIDER: &str = "provider";
    pub const MODEL: &str = "model";
    pub const TURN_INDEX: &str = "turn.index";
    pub const TURN_INPUT_TOKENS: &str = "turn.input_tokens";
    pub const TURN_OUTPUT_TOKENS: &str = "turn.output_tokens";
    pub const LLM_PROVIDER: &str = "llm.provider";
    pub const LLM_MODEL: &str = "llm.model";
    pub const LLM_STOP_REASON: &str = "llm.stop_reason";
    pub const LLM_CACHED_TOKENS: &str = "llm.cached_tokens";
    pub const TOOL_NAME: &str = "tool.name";
    pub const TOOL_SIDE_EFFECT: &str = "tool.side_effect";
    pub const TOOL_PARALLEL: &str = "tool.parallel";
    pub const TOOL_OK: &str = "tool.ok";
    pub const TOOL_ELAPSED_MS: &str = "tool.elapsed_ms";
    pub const TOOL_TRUNCATED: &str = "tool.truncated";
    pub const PERMISSION_VERDICT: &str = "permission.verdict";
    pub const PERMISSION_MATCHED_RULE: &str = "permission.matched_rule";
    pub const COMPRESS_LEVEL: &str = "compress.level";
    pub const COMPRESS_TOKENS_BEFORE: &str = "compress.tokens_before";
    pub const COMPRESS_TOKENS_AFTER: &str = "compress.tokens_after";
    // === 以下为可观测性补充（见 docs/observability.md §4.2） ===
    /// 记忆类型（`long_term`/`auto`/`session`/`project_doc`）。
    pub const MEMORY_TYPE: &str = "memory.type";
    /// Journal 操作类型（`record`/`undo`/`diff`）。
    pub const JOURNAL_OP: &str = "journal.op";
    /// MCP server 名。
    pub const MCP_SERVER: &str = "mcp.server";
    /// MCP 工具名。
    pub const MCP_TOOL: &str = "mcp.tool";
    /// 重试当前次数。
    pub const RETRY_ATTEMPT: &str = "retry.attempt";
    /// 重试最大次数。
    pub const RETRY_MAX: &str = "retry.max";
    /// 错误类别。
    pub const ERROR_CATEGORY: &str = "error.category";
    /// 错误码。
    pub const ERROR_CODE: &str = "error.code";
}

/// `OTel` 采样策略（见 `design.md` §15.3）。
///
/// 由 `OTEL_TRACES_SAMPLER` 环境变量控制：
/// - `always_on`（默认，本地调试）：全量采样；
/// - `traceidratio`（生产）：按比例采样，配合 `OTEL_TRACES_SAMPLER_ARG` 配比例（默认 0.1）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sampler {
    /// 全量采样（本地调试默认）。
    AlwaysOn,
    /// 按比例采样（生产默认 0.1）。
    TraceIdRatio(f64),
}

impl Sampler {
    /// 从环境变量解析采样策略。
    ///
    /// - `OTEL_TRACES_SAMPLER=traceidratio` → `TraceIdRatio(arg)`，`arg` 取
    ///   `OTEL_TRACES_SAMPLER_ARG`（默认 0.1）；
    /// - 其余（含未设置、`always_on`）→ `AlwaysOn`。
    #[must_use]
    pub fn from_env() -> Self {
        // SAFETY: 单线程启动期调用，无并发竞争
        let sampler_env = std::env::var("OTEL_TRACES_SAMPLER").ok();
        match sampler_env.as_deref() {
            Some("traceidratio") => {
                let ratio = std::env::var("OTEL_TRACES_SAMPLER_ARG")
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.1)
                    .clamp(0.0, 1.0);
                Self::TraceIdRatio(ratio)
            }
            _ => Self::AlwaysOn,
        }
    }
}

/// 是否配置了 OTLP 端点（`OTEL_EXPORTER_OTLP_ENDPOINT`）。
///
/// CLI 据此决定是否安装 OTLP layer；未配置时降级为纯本地 fmt 日志（M0 验收）。
#[must_use]
pub fn otlp_endpoint_configured() -> bool {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok()
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)] // Rust 2024 中 set_var/remove_var 标记为 unsafe
    use super::*;
    use std::sync::Mutex;

    // 所有修改 OTEL_* 环境变量的测试共享此锁，强制串行执行（env var 是进程全局）。
    // 不加锁时并行测试会相互覆盖 OTEL_TRACES_SAMPLER_ARG，导致 default_ratio 测试
    // 读到 parses_arg 测试残留的 0.3 而非默认 0.1。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn sampler_default_is_always_on() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        // SAFETY: 持有 ENV_LOCK 保证串行，无并发 set_var 风险。
        unsafe {
            std::env::remove_var("OTEL_TRACES_SAMPLER");
            std::env::remove_var("OTEL_TRACES_SAMPLER_ARG");
        }
        assert_eq!(Sampler::from_env(), Sampler::AlwaysOn);
    }

    #[test]
    fn sampler_traceidratio_parses_arg() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        // SAFETY: 持有 ENV_LOCK 保证串行。
        unsafe {
            std::env::set_var("OTEL_TRACES_SAMPLER", "traceidratio");
            std::env::set_var("OTEL_TRACES_SAMPLER_ARG", "0.3");
        }
        assert_eq!(Sampler::from_env(), Sampler::TraceIdRatio(0.3));
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("OTEL_TRACES_SAMPLER");
            std::env::remove_var("OTEL_TRACES_SAMPLER_ARG");
        }
    }

    #[test]
    fn sampler_traceidratio_default_ratio() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        // SAFETY: 持有 ENV_LOCK 保证串行，先 set 再 remove ARG，确保读到默认值。
        unsafe {
            std::env::set_var("OTEL_TRACES_SAMPLER", "traceidratio");
            std::env::remove_var("OTEL_TRACES_SAMPLER_ARG");
        }
        assert_eq!(Sampler::from_env(), Sampler::TraceIdRatio(0.1));
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("OTEL_TRACES_SAMPLER");
        }
    }

    #[test]
    fn sampler_traceidratio_clamps_invalid() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        // SAFETY: 持有 ENV_LOCK 保证串行。
        unsafe {
            std::env::set_var("OTEL_TRACES_SAMPLER", "traceidratio");
            std::env::set_var("OTEL_TRACES_SAMPLER_ARG", "2.5");
        }
        assert_eq!(Sampler::from_env(), Sampler::TraceIdRatio(1.0));
        // SAFETY: 同上。
        unsafe {
            std::env::set_var("OTEL_TRACES_SAMPLER_ARG", "-0.5");
        }
        assert_eq!(Sampler::from_env(), Sampler::TraceIdRatio(0.0));
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("OTEL_TRACES_SAMPLER");
            std::env::remove_var("OTEL_TRACES_SAMPLER_ARG");
        }
    }

    #[test]
    fn span_name_constants_are_stable() {
        // 防止意外重命名破坏 OTel 后端查询
        assert_eq!(span_name::SESSION, "session");
        assert_eq!(span_name::TURN, "turn");
        assert_eq!(span_name::LLM_CHAT_STREAM, "llm.chat_stream");
        assert_eq!(span_name::TOOL_CALL, "tool.call");
        assert_eq!(span_name::PERMISSION_CHECK, "permission.check");
        assert_eq!(span_name::COMPRESS, "compress");
        // 可观测性补充（docs/observability.md §4.1）
        assert_eq!(span_name::MEMORY_LOAD, "memory.load");
        assert_eq!(span_name::MEMORY_SAVE, "memory.save");
        assert_eq!(span_name::JOURNAL_RECORD, "journal.record");
        assert_eq!(span_name::JOURNAL_UNDO, "journal.undo");
        assert_eq!(span_name::STORAGE_APPEND, "storage.append");
        assert_eq!(span_name::AUDIT_RECORD, "audit.record");
        assert_eq!(span_name::SANDBOX_APPLY, "sandbox.apply");
        assert_eq!(span_name::MCP_CALL, "mcp.call");
        assert_eq!(span_name::MCP_CONNECT, "mcp.connect");
        assert_eq!(span_name::CONTEXT_BUILD, "context.build");
        assert_eq!(span_name::PROVIDER_RETRY, "provider.retry");
        assert_eq!(span_name::CONFIG_RELOAD, "config.reload");
        assert_eq!(span_name::EVENT_PUBLISH, "event.publish");
        assert_eq!(span_name::SHELL_BG_SPAWN, "shell.bg_spawn");
    }

    #[test]
    fn otlp_endpoint_detection_respects_env() {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        // SAFETY: 持有 ENV_LOCK 保证串行。
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
        assert!(!otlp_endpoint_configured());
        // SAFETY: 同上。
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318");
        }
        assert!(otlp_endpoint_configured());
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
    }
}
