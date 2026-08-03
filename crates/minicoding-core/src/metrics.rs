//! Metrics 接口定义（可度量）。
//!
//! 设计意图（见 `docs/observability.md` §3）：
//! - **不引入新依赖**：metrics 记录为带 `target: "metrics::*"` 的 `tracing` 事件，
//!   复用现有 `tracing` subscriber 基础设施。
//! - **降级安全**：无 subscriber 时静默丢弃，不影响业务逻辑。
//! - **结构化字段**：所有 metrics 事件使用结构化字段，便于后端聚合查询。
//!
//! 消费方式：
//! - 本地调试：fmt subscriber 输出到 stderr
//! - 生产：OTLP layer 转发到 `OTel` Collector → Prometheus/Jaeger
//! - 自定义：注册 layer 拦截 `metrics::*` target

use std::time::Duration;

// === Counter（计数器）===

/// LLM token 消耗计数。
///
/// # 参数
/// - `provider`：LLM provider 名（`openai`/`anthropic`/`ollama`）
/// - `direction`：方向（`input`/`output`/`cached`）
/// - `count`：token 数量
pub fn record_llm_tokens(provider: &str, direction: &str, count: u64) {
    if count > 0 {
        tracing::debug!(
            target: "metrics::llm_tokens_total",
            provider = provider,
            direction = direction,
            count = count,
            "LLM token consumed"
        );
    }
}

/// 工具调用计数。
///
/// # 参数
/// - `tool`：工具名（`fs.read`/`shell.run` 等）
/// - `side_effect`：副作用类型（`none`/`file_write`/`command`）
/// - `result`：执行结果（`ok`/`err`/`timeout`/`cancelled`）
pub fn record_tool_call(tool: &str, side_effect: &str, result: &str) {
    tracing::debug!(
        target: "metrics::tool_calls_total",
        tool = tool,
        side_effect = side_effect,
        result = result,
        "tool call completed"
    );
}

/// 权限决策计数。
///
/// # 参数
/// - `verdict`：决策结果（`allow`/`deny`/`ask`/`allow_always`/`deny_always`）
pub fn record_permission(verdict: &str) {
    tracing::debug!(
        target: "metrics::permission_decisions_total",
        verdict = verdict,
        "permission decision made"
    );
}

/// 上下文压缩计数。
///
/// # 参数
/// - `level`：压缩级别（`1`/`2`/`3`/`4`）
/// - `result`：压缩结果（`ok`/`err`/`skipped`）
pub fn record_compress(level: u8, result: &str) {
    tracing::debug!(
        target: "metrics::compress_invocations_total",
        level = level,
        result = result,
        "compression completed"
    );
}

/// Hook 执行计数。
///
/// # 参数
/// - `hook`：Hook 名
/// - `event`：触发事件（`pre_tool_call`/`post_tool_call` 等）
/// - `result`：执行结果（`ok`/`err`/`deny`）
pub fn record_hook(hook: &str, event: &str, result: &str) {
    tracing::debug!(
        target: "metrics::hook_executions_total",
        hook = hook,
        event = event,
        result = result,
        "hook executed"
    );
}

/// 错误计数。
///
/// # 参数
/// - `category`：错误类别（`llm`/`tool`/`permission`/`sandbox`/`mcp`/`storage`/`hook`/`context`/`journal`）
pub fn record_error(category: &str) {
    tracing::error!(
        target: "metrics::errors_total",
        category = category,
        "error occurred"
    );
}

/// MCP 工具调用计数。
///
/// # 参数
/// - `server`：MCP server 名
/// - `tool`：工具名
/// - `result`：执行结果（`ok`/`err`/`timeout`）
pub fn record_mcp_tool_call(server: &str, tool: &str, result: &str) {
    tracing::debug!(
        target: "metrics::mcp_tool_calls_total",
        server = server,
        tool = tool,
        result = result,
        "MCP tool call completed"
    );
}

// === Histogram（直方图）===

/// 延迟记录（直方图）。
///
/// # 参数
/// - `name`：指标名（`llm_call_duration_ms`/`tool_call_duration_ms` 等）
/// - `label_key`：标签键（`provider`/`tool` 等）
/// - `label_val`：标签值
/// - `ms`：延迟毫秒数
pub fn record_duration_ms(name: &str, label_key: &str, label_val: &str, ms: u64) {
    tracing::debug!(
        target: "metrics::duration_ms",
        metric = name,
        label_key = label_key,
        label_val = label_val,
        ms = ms,
        "duration recorded"
    );
}

/// 上下文 token 数记录（直方图）。
///
/// # 参数
/// - `phase`：阶段（`before_compress`/`after_compress`/`request`）
/// - `count`：token 数
pub fn record_context_tokens(phase: &str, count: u64) {
    tracing::debug!(
        target: "metrics::context_tokens",
        phase = phase,
        count = count,
        "context token count"
    );
}

// === Gauge（仪表盘）===

/// 熔断状态变更。
///
/// # 参数
/// - `breaker_type`：熔断器类型（`compress`/`sandbox`）
/// - `state`：状态（`normal`/`warning`/`fused`）
pub fn set_circuit_breaker(breaker_type: &str, state: &str) {
    tracing::info!(
        target: "metrics::circuit_breaker_state",
        r#type = breaker_type,
        state = state,
        "circuit breaker state changed"
    );
}

/// 活跃会话数变更。
///
/// # 参数
/// - `count`：当前活跃会话数
pub fn set_active_sessions(count: u64) {
    tracing::debug!(
        target: "metrics::active_sessions",
        count = count,
        "active sessions updated"
    );
}

/// MCP 连接数变更。
///
/// # 参数
/// - `server`：MCP server 名
/// - `count`：当前连接数
pub fn set_mcp_connections(server: &str, count: u64) {
    tracing::debug!(
        target: "metrics::mcp_connections",
        server = server,
        count = count,
        "MCP connections updated"
    );
}

/// 后台 Shell 数量变更。
///
/// # 参数
/// - `count`：当前后台 shell 数
pub fn set_background_shells(count: u64) {
    tracing::debug!(
        target: "metrics::background_shells",
        count = count,
        "background shells updated"
    );
}

// === 便利宏 ===

/// 记录操作的延迟和结果。
///
/// 用法：
/// ```ignore
/// let timer = metrics::start_timer();
/// let result = tool.execute(...).await;
/// metrics::record_operation("tool_call", "tool", tool.name(), timer, &result);
/// ```
#[must_use]
pub fn start_timer() -> std::time::Instant {
    std::time::Instant::now()
}

/// 记录操作延迟（从 `Instant` 计算）。
pub fn record_elapsed(name: &str, label_key: &str, label_val: &str, start: std::time::Instant) {
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    record_duration_ms(name, label_key, label_val, ms);
}

/// 记录操作延迟（从 `Duration` 计算）。
pub fn record_duration(name: &str, label_key: &str, label_val: &str, duration: Duration) {
    let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    record_duration_ms(name, label_key, label_val, ms);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn record_llm_tokens_does_not_panic() {
        // 仅验证不 panic，不验证 subscriber 行为
        record_llm_tokens("openai", "input", 100);
        record_llm_tokens("openai", "output", 50);
        record_llm_tokens("openai", "input", 0); // 零计数不记录
    }

    #[test]
    fn record_tool_call_does_not_panic() {
        record_tool_call("fs.read", "none", "ok");
        record_tool_call("shell.run", "command", "err");
    }

    #[test]
    fn record_permission_does_not_panic() {
        record_permission("allow");
        record_permission("deny");
        record_permission("ask");
    }

    #[test]
    fn record_compress_does_not_panic() {
        record_compress(1, "ok");
        record_compress(2, "err");
        record_compress(3, "skipped");
    }

    #[test]
    fn record_error_does_not_panic() {
        record_error("llm");
        record_error("tool");
        record_error("sandbox");
    }

    #[test]
    fn record_duration_calculates_ms() {
        let start = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        record_elapsed("test_metric", "tool", "test", start);
    }

    #[test]
    fn set_circuit_breaker_does_not_panic() {
        set_circuit_breaker("compress", "normal");
        set_circuit_breaker("compress", "fused");
        set_circuit_breaker("sandbox", "warning");
    }
}
