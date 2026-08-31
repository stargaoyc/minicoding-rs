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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

// === 进程内聚合注册表（P9：/metrics 暴露用）===

/// 全局计数器注册表：`record_*` 系列在发 tracing 事件的同时累加此表，
/// 供 `snapshot_prometheus()` 渲染 `/metrics` 端点。BTreeMap 保证输出稳定排序。
///
/// 全局状态违反 `architecture.md:12`「组件无全局可变状态」，但 Prometheus 指标
/// 注册表是行业最佳实践（opentelemetry 亦全局），`#[cfg(test)]` 用 `reset_metrics`
/// 清空保证测试隔离（R10 P2 §17）。
static REGISTRY: LazyLock<Mutex<BTreeMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// gauge 注册表（ENG-5，2026-08-26 R3 审查）：与 counter 分表——gauge 语义是
/// "覆盖当前值"，此前 `set_active_sessions` 复用累加表，连续 set(5)、set(3)
/// 得到 8 且被渲染成 `# TYPE counter`（语义双错）。
static GAUGES: LazyLock<Mutex<BTreeMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// 累加一个计数器（内部辅助）。
fn bump(counter: &str, labels: &str, n: u64) {
    if let Ok(mut reg) = REGISTRY.lock() {
        *reg.entry(format!("{counter}{labels}")).or_insert(0) += n;
    }
}

/// 设置一个 gauge 值（覆盖语义，内部辅助）。
fn set_gauge(gauge: &str, labels: &str, value: u64) {
    if let Ok(mut reg) = GAUGES.lock() {
        reg.insert(format!("{gauge}{labels}"), value);
    }
}

/// 渲染 Prometheus text format（0.0.4）快照。
///
/// 计数器名已带标签后缀（`name{label="v"}` 形态由记录方拼好），此处仅补
/// `# TYPE <base> <type>` 行按 base 名去重输出（counter 与 gauge 分区渲染）。
#[must_use]
pub fn snapshot_prometheus() -> String {
    let mut out = String::new();
    for (reg, ty) in [(REGISTRY.lock(), "counter"), (GAUGES.lock(), "gauge")] {
        let Ok(reg) = reg else {
            continue;
        };
        let mut last_base = String::new();
        for (key, val) in reg.iter() {
            let base = key.split('{').next().unwrap_or(key).to_string();
            if base != last_base {
                let _ = writeln!(out, "# TYPE {base} {ty}");
                last_base = base;
            }
            let _ = writeln!(out, "{key} {val}");
        }
    }
    out
}

/// 重置所有指标计数器（`#[cfg(test)]` 使用，保证测试隔离）。
///
/// 测试快照前调用防止跨测试指标泄漏（R10 P2 §17）。
#[cfg(test)]
pub fn reset_metrics() {
    if let Ok(mut reg) = REGISTRY.lock() {
        reg.clear();
    }
    if let Ok(mut reg) = GAUGES.lock() {
        reg.clear();
    }
}

// === Counter（计数器）===

/// LLM token 消耗计数。
///
/// # 参数
/// - `provider`：LLM provider 名（`openai`/`anthropic`/`ollama`）
/// - `direction`：方向（`input`/`output`/`cached`）
/// - `count`：token 数量
pub fn record_llm_tokens(provider: &str, direction: &str, count: u64) {
    bump(
        "minicoding_llm_tokens_total",
        &format!("{{provider=\"{provider}\",direction=\"{direction}\"}}"),
        count,
    );
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
    bump(
        "minicoding_tool_calls_total",
        &format!(r#"{{tool="{tool}",result="{result}"}}"#),
        1,
    );
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
    bump(
        "minicoding_permissions_total",
        &format!("{{verdict=\"{verdict}\"}}"),
        1,
    );
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
    bump(
        "minicoding_compress_total",
        &format!("{{level=\"{level}\",result=\"{result}\"}}"),
        1,
    );
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
    bump(
        "minicoding_hook_runs_total",
        &format!(r#"{{hook="{hook}",event="{event}"}}"#),
        1,
    );
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
    bump(
        "minicoding_errors_total",
        &format!("{{category=\"{category}\"}}"),
        1,
    );
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
    // ENG-5：此前只发 tracing 事件、从不进注册表——/metrics 永远看不到 MCP 计数
    bump(
        "minicoding_mcp_tool_calls_total",
        &format!(r#"{{server="{server}",result="{result}"}}"#),
        1,
    );
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
    set_gauge("minicoding_active_sessions", "", count);
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
    set_gauge(
        "minicoding_mcp_connections",
        &format!(r#"{{server="{server}"}}"#),
        count,
    );
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
    set_gauge("minicoding_background_shells", "", count);
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

    // F1：start_paused 虚拟时钟——原 std::thread::sleep(10ms) 仅为了让
    // elapsed 非零；本测试只验证 record_elapsed 不 panic，虚拟推进等价。
    #[tokio::test(start_paused = true)]
    async fn record_duration_calculates_ms() {
        let start = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        record_elapsed("test_metric", "tool", "test", start);
    }

    #[test]
    fn set_circuit_breaker_does_not_panic() {
        set_circuit_breaker("compress", "normal");
        set_circuit_breaker("compress", "fused");
        set_circuit_breaker("sandbox", "warning");
    }

    // ENG-5（2026-08-26 R3 审查）：gauge 覆盖语义 + MCP 计数进 /metrics
    #[test]
    fn gauges_use_overwrite_semantics_and_render_as_gauge() {
        set_active_sessions(5);
        set_active_sessions(3);
        let snap = snapshot_prometheus();
        // 取最后一次覆盖值 3，而非累加值 8
        assert!(
            snap.contains("minicoding_active_sessions 3\n"),
            "gauge 应为覆盖语义: {snap}"
        );
        assert!(
            snap.contains("# TYPE minicoding_active_sessions gauge"),
            "应渲染为 gauge 类型"
        );
        record_mcp_tool_call("fs-server", "fs.read", "ok");
        assert!(snap_prometheus_contains_mcp(), "MCP 计数应进入 /metrics");
    }

    fn snap_prometheus_contains_mcp() -> bool {
        snapshot_prometheus().contains("minicoding_mcp_tool_calls_total")
    }
}
