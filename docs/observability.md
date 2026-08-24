# 可观测性设计文档（Observability Design）

> 本文档定义 `minicoding-rs` 的全链路可观测性体系：**可追踪**（tracing）+ **可度量**（metrics）+ **可审计**（audit）。
> 运行时大模型约束见 `docs/rules.md`；技术选型见 `docs/tech-stack.md`；架构设计见 `docs/design.md` §15。

---

## §1 目标与原则

### 1.1 三大支柱

| 支柱 | 职责 | 实现层 |
|------|------|--------|
| **Tracing** | 全链路追踪，每一步执行可定位、可关联 | `tracing` + OTel span |
| **Metrics** | 可度量，关键指标可聚合、可告警 | `tracing` events + metrics target |
| **Audit** | 可审计，安全决策落盘不可篡改 | `audit.log` JSONL（见 `docs/security.md`） |

### 1.2 设计原则

1. **业务代码只写 `tracing`**：业务 crate 不直接依赖 `opentelemetry` SDK，只使用 `tracing` 宏。由 subscriber 层（`minicoding-cli`）桥接到 OTLP 或本地 fmt（见 `otel.rs` 设计意图）。
2. **core 保持轻量**：`minicoding-core` 只提供 span 名/属性常量、metrics 接口定义和采样配置。不引入网络/订阅器重依赖（AGENTS.md §3.5）。
3. **降级安全**：未配置 OTLP 端点时降级为纯本地 fmt 日志，不报错（M0 验收标准）。
4. **全链路不断裂**：Agent 循环 → 工具执行 → 领域 crate 的 span 父子关系完整，无断裂点。
5. **结构化字段优先**：所有 span 和 metrics 事件使用结构化字段（`field = value`），便于后端聚合查询。

### 1.3 与现有文档的关系

- `docs/design.md` §15：OTel 初始设计（span 名、采样策略、资源属性）
- `docs/security.md`：审计日志、权限决策记录
- `docs/rules.md` C-29/C-30：压缩熔断/沙箱拒绝不可被 LLM 绕过，需可观测
- `docs/modules.md` §15.7：各 crate 的可观测性职责

本文档**扩展** `design.md` §15，补充 metrics 设计和 span 覆盖盲区。

---

## §2 现状评估

### 2.1 已覆盖

| 维度 | 现状 | 位置 |
|------|------|------|
| OTel 框架 | 完整初始化，OTLP exporter + fmt 降级 | `cli/otel_init.rs`、`core/otel.rs` |
| 采样策略 | AlwaysOn / TraceIdRatio 可配 | `otel.rs:60-103` |
| 关键路径 span | turn / llm_call / tool_call / permission / compress(4级) / hook.run / undo | `rt.rs`、`context/manager.rs`、`hooks/trait_def.rs` |
| EventBus 事件 | Token/MessageAppended/TurnEnd/ToolCallStarted-Finished 等 10 种 | `runtime/event.rs` |
| 审计落盘 | JSONL audit.log，0600 权限 | `storage/audit.rs` |
| span 命名规范 | core/otel.rs 统一定义 20 个 span 名 + 30 个属性键（2026-08-23 审查核对） | `otel.rs:18-110` |
| Metrics 接口 | Counter/Histogram/Gauge 共 14 个指标函数 | `core/metrics.rs` |
| Span 断裂修复 | 13 处断裂全部补齐（memory/journal/storage/mcp/sandbox/event 等） | 见 §4.3 |
| Span 属性完整 | llm_call/tool_call/permission/compress 四类 span 属性已补全 | `rt.rs`、`context/manager.rs` |
| 错误可观测性 | 9 类错误分类（llm/tool/permission/sandbox/mcp/storage/hook/context/journal） | 18 处 `record_error` 调用点 |

### 2.2 已完成（原"缺失"项的修复状态）

#### 2.2.1 Metrics 模块（P0，已完成）

`crates/minicoding-core/src/metrics.rs` 已实现 14 个指标函数（7 Counter + 2 Histogram + 5 Gauge），
覆盖 LLM token / 工具调用 / 权限决策 / 压缩 / Hook / MCP / 错误 / 熔断状态 / 活跃会话 / 后台 shell 等。
调用点已接入 `rt.rs`/`context/manager.rs`/`hooks/trait_def.rs`/`mcp/client/rmcp.rs`/`tools/shell/background.rs`/`server/session_mgr.rs`。

#### 2.2.2 Span 覆盖盲区（P1，已完成）

原 13 处断裂点全部补齐：

| # | 断裂路径 | 位置 | span 名 | 状态 |
|---|---------|------|---------|------|
| 1 | Memory load/save | `memory/long_term.rs` | `memory.load` / `memory.save` | 已补齐 |
| 2 | 会话摘要生成 | `memory/session_sum.rs` | `llm.chat_stream`（复用） | 已补齐 |
| 3 | Auto memory 操作 | `memory/auto.rs` | `memory.load` / `memory.save` | 已补齐 |
| 4 | 项目文档加载 | `memory/project_doc/loader.rs` | `memory.load` | 已补齐 |
| 5 | Journal record/undo | `journal/journal_impl.rs` | `journal.record` / `journal.undo` | 已补齐 |
| 6 | JSONL 存储 | `storage/jsonl.rs` | `storage.append` / `storage.load` | 已补齐 |
| 7 | AuditSink record | `storage/audit.rs` | `audit.record` | 已补齐 |
| 8 | MCP 连接/调用 | `mcp/client/rmcp.rs` | `mcp.connect` / `mcp.call` | 已补齐 |
| 9 | 上下文压缩 | `context/manager.rs` compress | `compress` | 已补齐 |
| 10 | Provider retry | `providers/common/retry.rs` | `provider.retry` | 已补齐 |
| 11 | 配置热重载 | `core/config/watcher.rs` | `config.reload` | 已补齐 |
| 12 | EventBus publish | `core/runtime/event.rs` | `event.publish` | 已补齐 |
| 13 | 后台 Shell 操作 | `tools/shell/background.rs` | `shell.bg_spawn` | 已补齐 |

#### 2.2.3 Span 属性不完整（P2，已完成）

`rt.rs` 与 `context/manager.rs` 的关键 span 已补全结构化字段：

- `llm_call` span：补 `llm.provider` / `llm.model` / `message_count`
- `tool_call` span（readonly 桶）：补 `tool.name` / `tool.side_effect = "none"` / `tool.parallel = true`
- `tool_call` span（side_effect 桶）：补 `tool.name` / `tool.side_effect` / `tool.parallel = false`
- `permission` span：补 `tool.name` / `tool.side_effect` / `permission.verdict`（动态填充）
- `compress` span：补 `compress.tokens_before` / `compress.tokens_after`（动态填充）

#### 2.2.4 错误可观测性（P3，已完成）

- 错误日志通过 `tracing` 宏自动关联当前 span context
- 错误率指标通过 `metrics::record_error(category)` 计数，已接入 18 处错误路径
- 错误分类统计覆盖 9 类（llm/tool/permission/sandbox/mcp/storage/hook/context/journal）

---

## §3 Metrics 设计

### 3.1 设计方案

采用 **`tracing` events with metrics target** 方案，而非引入 `opentelemetry-api` metrics 依赖：

- **原理**：metrics 记录为带 `target: "metrics::*"` 的 `tracing::debug!` 事件，携带结构化字段
- **优势**：
  1. 不引入新依赖到 core（保持轻量）
  2. 复用现有 `tracing` subscriber 基础设施
  3. 可被 OTel metrics layer 或 Prometheus exporter 消费
  4. 降级安全：无 subscriber 时静默丢弃，不影响业务

### 3.2 指标清单

#### 3.2.1 Counter（计数器）

| 指标名 | 标签 | 触发点 | 用途 |
|--------|------|--------|------|
| `llm_tokens_total` | `provider`, `direction`(input/output) | LLM 调用完成 | token 消耗计费 |
| `tool_calls_total` | `tool`, `side_effect`, `result`(ok/err) | 工具执行完成 | 工具调用频率/成功率 |
| `permission_decisions_total` | `verdict`(allow/deny/ask/allow_always) | 权限决策完成 | 权限决策分布 |
| `compress_invocations_total` | `level`(1-4), `result`(ok/err/skipped) | 压缩完成 | 压缩触发次数/成功率 |
| `hook_executions_total` | `hook`, `event`, `result` | Hook 执行完成 | Hook 执行统计 |
| `errors_total` | `category`(llm/tool/permission/sandbox/mcp/storage) | 错误发生 | 错误率告警 |
| `mcp_tool_calls_total` | `server`, `tool`, `result` | MCP 工具调用完成 | MCP 使用统计 |

#### 3.2.2 Histogram（直方图）

| 指标名 | 标签 | 触发点 | 用途 |
|--------|------|--------|------|
| `llm_call_duration_ms` | `provider` | LLM 调用完成 | LLM 延迟分布 |
| `tool_call_duration_ms` | `tool` | 工具执行完成 | 工具延迟分布 |
| `context_tokens` | `phase`(before_compress/after_compress/request) | 上下文构建/压缩 | token 分布 |
| `permission_decision_duration_ms` | `verdict` | 权限决策完成 | 权限延迟分布 |

#### 3.2.3 Gauge（仪表盘）

| 指标名 | 标签 | 更新点 | 用途 |
|--------|------|--------|------|
| `active_sessions` | - | 会话开始/结束 | 活跃会话数 |
| `circuit_breaker_state` | `type`(compress/sandbox) | 熔断状态变化 | 熔断监控 |
| `mcp_connections` | `server` | 连接建立/断开 | MCP 连接数 |
| `context_window_usage` | `session` | 上下文构建 | 窗口使用率 |
| `background_shells` | - | spawn/kill | 后台 shell 数 |

### 3.3 接口定义

```rust
// core/metrics.rs

/// LLM token 消耗计数。
pub fn record_llm_tokens(provider: &str, direction: &str, count: u64) {
    tracing::debug!(
        target: "metrics::llm_tokens_total",
        provider = provider,
        direction = direction,
        count = count,
        "LLM token consumed"
    );
}

/// 工具调用计数。
pub fn record_tool_call(tool: &str, side_effect: &str, result: &str) {
    tracing::debug!(
        target: "metrics::tool_calls_total",
        tool = tool,
        side_effect = side_effect,
        result = result,
        "tool call completed"
    );
}

/// 延迟记录（直方图）。
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

/// 熔断状态变更（Gauge）。
pub fn set_circuit_breaker(breaker_type: &str, state: &str) {
    tracing::info!(
        target: "metrics::circuit_breaker_state",
        r#type = breaker_type,
        state = state,
        "circuit breaker state changed"
    );
}
```

### 3.4 消费方式

- **本地调试**：fmt subscriber 输出 metrics 事件到 stderr
- **生产**：OTLP layer 将 metrics 事件转发到 OTel Collector，由 Collector 转发到 Prometheus/Jaeger
- **自定义**：用户可注册 `tracing_subscriber` layer 拦截 `metrics::*` target 做自定义处理

---

## §4 Span 覆盖设计

### 4.1 span 名补充

在 `otel.rs` 的 `span_name` 模块补充：

```rust
pub mod span_name {
    // === 现有 ===
    pub const SESSION: &str = "session";
    pub const TURN: &str = "turn";
    pub const LLM_CHAT_STREAM: &str = "llm.chat_stream";
    pub const TOOL_CALL: &str = "tool.call";
    pub const PERMISSION_CHECK: &str = "permission.check";
    pub const COMPRESS: &str = "compress";

    // === 新增 ===
    pub const MEMORY_LOAD: &str = "memory.load";
    pub const MEMORY_SAVE: &str = "memory.save";
    pub const JOURNAL_RECORD: &str = "journal.record";
    pub const JOURNAL_UNDO: &str = "journal.undo";
    pub const STORAGE_APPEND: &str = "storage.append";
    pub const AUDIT_RECORD: &str = "audit.record";
    pub const SANDBOX_APPLY: &str = "sandbox.apply";
    pub const MCP_CALL: &str = "mcp.call";
    pub const MCP_CONNECT: &str = "mcp.connect";
    pub const CONTEXT_BUILD: &str = "context.build";
    pub const PROVIDER_RETRY: &str = "provider.retry";
    pub const CONFIG_RELOAD: &str = "config.reload";
    pub const EVENT_PUBLISH: &str = "event.publish";
    pub const SHELL_BG_SPAWN: &str = "shell.bg_spawn";
}
```

### 4.2 span 属性补充

在 `otel.rs` 的 `attr` 模块补充：

```rust
pub mod attr {
    // === 现有 23 个属性键 ===

    // === 新增 ===
    pub const MEMORY_TYPE: &str = "memory.type";       // long_term/auto/session
    pub const JOURNAL_OP: &str = "journal.op";         // record/undo/diff
    pub const MCP_SERVER: &str = "mcp.server";
    pub const MCP_TOOL: &str = "mcp.tool";
    pub const RETRY_ATTEMPT: &str = "retry.attempt";
    pub const RETRY_MAX: &str = "retry.max";
    pub const ERROR_CATEGORY: &str = "error.category";
    pub const ERROR_CODE: &str = "error.code";
}
```

### 4.3 各断裂点修复方案

每个断裂点使用 `#[tracing::instrument]` 或手动 `span` 补齐。优先用 `#[instrument]`（自动传播 parent span）。

| # | 文件 | 函数 | span 名 | 关键属性 |
|---|------|------|---------|---------|
| 1 | `memory/long_term.rs` | `load` / `save` | `memory.load` / `memory.save` | `memory.type` |
| 2 | `memory/session_sum.rs` | `summarize` | `llm.chat_stream`（复用） | `llm.purpose=summarize` |
| 3 | `memory/auto.rs` | `load_entries` / `add_entry` | `memory.load` / `memory.save` | `memory.type=auto` |
| 4 | `memory/project_doc/loader.rs` | `load` | `memory.load` | `memory.type=project_doc` |
| 5 | `journal/journal_impl.rs` | `record` / `undo` | `journal.record` / `journal.undo` | `journal.op` |
| 6 | `storage/jsonl.rs` | `append` / `load_messages_sync` | `storage.append` | - |
| 7 | `storage/audit.rs` | `record` | `audit.record` | `audit.kind` |
| 8 | `mcp/client/rmcp.rs` | `start_one` / `call_tool` | `mcp.connect` / `mcp.call` | `mcp.server`, `mcp.tool` |
| 9 | `context/manager.rs` | `compress` / `build_chat_request` | `compress` / `context.build` | `compress.level` |
| 10 | `providers/common/retry.rs` | `attempt` / `chat_stream` | `provider.retry` | `retry.attempt`, `retry.max` |
| 11 | `core/config/watcher.rs` | `poll` / reload callback | `config.reload` | - |
| 12 | `core/runtime/event.rs` | `publish` | `event.publish` | `event.kind` |
| 13 | `tools/shell/background.rs` | `spawn` / `output` | `shell.bg_spawn` | `shell.id` |

---

## §5 Span 属性标准化

### 5.1 rt.rs 现有 span 增强

在 `rt.rs` 的关键 span 中补充结构化属性：

```rust
// llm_call span 增强
tracing::info_span!(
    "llm_call",
    session.id = %session_id,
    llm.provider = %provider_name,
    llm.model = %model_name,
    otel.name = otel::span_name::LLM_CHAT_STREAM,
);

// tool_call span 增强
tracing::debug_span!(
    "tool_call",
    session.id = %session_id,
    tool.name = %tool.name(),
    tool.side_effect = ?tool.side_effect(),
    tool.parallel = is_parallel,
    otel.name = otel::span_name::TOOL_CALL,
);

// permission span 增强
tracing::info_span!(
    "permission",
    session.id = %session_id,
    tool.name = %tool_name,
    permission.verdict = %verdict,
    otel.name = otel::span_name::PERMISSION_CHECK,
);
```

### 5.2 属性传播规则

1. **session.id 全局传播**：所有 span 必须包含 `session.id`，便于按会话过滤
2. **otel.name 统一标注**：所有 span 添加 `otel.name = span_name::XXX`，确保 OTel 后端查询一致
3. **枚举用 Display**：`SideEffect`/`Verdict` 等枚举用 `%var`（Display）而非 `?var`（Debug），保证字段值可读

---

## §6 错误可观测性

### 6.1 错误上报规范

所有错误路径必须：

1. **关联 span**：使用 `tracing::error!` 时自动关联当前 span context
2. **分类标记**：添加 `error.category` 字段，便于聚合
3. **记录 metrics**：调用 `metrics::record_error(category)` 计数

```rust
// 统一错误上报模式
match result {
    Ok(v) => v,
    Err(e) => {
        let category = error_category(&e); // llm/tool/permission/sandbox/mcp/storage
        tracing::error!(
            error = %e,
            error.category = category,
            otel.status_code = "ERROR",
            "operation failed"
        );
        minicoding_core::metrics::record_error(&category);
        return Err(e);
    }
}
```

### 6.2 错误分类

| category | 来源 | 触发条件 |
|----------|------|---------|
| `llm` | `LlmError` | LLM API 调用失败 |
| `tool` | `ToolError` | 工具执行失败 |
| `permission` | `PolicyError` | 权限决策异常 |
| `sandbox` | `SandboxError` | 沙箱拒绝/错误 |
| `mcp` | `McpError` | MCP 协议错误 |
| `storage` | `StorageError` | 存储 IO 错误 |
| `hook` | `HookError` | Hook 执行错误 |
| `context` | `RuntimeError` | 上下文压缩/熔断 |
| `journal` | `JournalError` | Journal 回滚错误 |

---

## §7 基础设施与接入（Docker 一键部署）

### 7.1 组件与选型

| 组件 | 选型 | 职责 |
|------|------|------|
| OTel Collector | `otel/opentelemetry-collector-contrib` | 统一 OTLP 入口（4317 gRPC / 4318 HTTP），batch + 内存限流，转发 trace/metrics |
| Trace 后端 | Jaeger v2（all-in-one） | 原生 OTLP 接收，span 存储与 UI 查询（16686） |
| Metrics 存储 | Prometheus | 抓取 collector 暴露的 metrics（8889），保留时长可配 |
| 统一 UI | Grafana | 预置 Prometheus + Jaeger datasource，统一查询 |

**选型依据**：与 `docs/tech-stack.md` §7 的 "Jaeger/Tempo/Grafana" 方向一致；collector 网关模式与本文 §3.4 的消费方式描述吻合（"OTLP layer 将 metrics 事件转发到 OTel Collector，由 Collector 转发到 Prometheus/Jaeger"）。Jaeger v2 原生支持 OTLP 接收，无需额外转换器。基础设施位于 `deploy/observability/`，默认只绑定 `127.0.0.1`（trace 数据含路径/会话信息，不应暴露到局域网）。

### 7.2 架构

```text
minicoding (宿主进程)                          docker compose (deploy/observability/)
┌──────────────────────┐   OTLP HTTP 4318   ┌──────────────────────────────┐
│ CLI / server /       │ ──────────────────▶ │ otel-collector  (batch+限流) │
│ desktop sidecar      │                     │   │  OTLP gRPC (容器内网)      │
└──────────────────────┘                     │   ▼                          │
                                             │ jaeger (16686 UI, 内存存储)  │
                                             │   │  /metrics :8889          │
                                             │   ▼                          │
                                             │ prometheus (9090) ◀─ grafana │
                                             └──────────────────────────────┘
```

架构说明：

- minicoding 进程在**宿主机**运行（不容器化），仅观测后端容器化；
- collector 是唯一上报入口，trace 经 OTLP gRPC 转发 Jaeger，metrics 以 `/metrics` 暴露给 Prometheus 抓取；
- Jaeger/Prometheus 端口映射到宿主机 `127.0.0.1`，Grafana 通过容器内网访问二者（datasource 预置）。

### 7.3 接入配置（环境变量）

minicoding 各入口通过**标准 OTel 环境变量**接入，无配置文件改动：

| 环境变量 | 默认 | 说明 |
|----------|------|------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | 未设置（不导出） | 上报端点，接入时设为 `http://127.0.0.1:4318` |
| `OTEL_TRACES_SAMPLER` | `always_on` | `traceidratio` 按比例采样（生产推荐） |
| `OTEL_TRACES_SAMPLER_ARG` | `0.1` | 采样比例（配合 `traceidratio`） |

入口支持矩阵：

| 入口 | 支持 | 说明 |
|------|------|------|
| CLI（`minicoding`） | ✅ `otel` feature（`full` 含） | `cli/otel_init.rs`，service.name = `minicoding` |
| Server（`minicoding-server`） | ✅ `otel` feature（默认启用） | `server/otel_init.rs`，service.name = `minicoding-server` |
| Desktop（Tauri sidecar） | ✅ 继承 | 经 sidecar 启动 server，读取宿主机环境变量 |

使用方式示例（CLI）：

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 minicoding --verbose
```

环境变量设置说明：接入后 span 在 Jaeger 中按 `service.name` 区分（`minicoding`/`minicoding-server`）。采样策略实现见 `core/otel.rs` 的 `Sampler::from_env`，OTLP 初始化见 `cli/otel_init.rs` 与 `server/otel_init.rs`（两者实现镜像，依赖方向约束见 `modules.md` §0.2，禁止 server 依赖 CLI）。

### 7.4 使用指引

1. **启动**：`cd deploy/observability && docker compose up -d`
2. **上报**：按 §7.3 设置环境变量后运行 minicoding（CLI / server / 桌面）
3. **查 trace**：Jaeger UI `http://127.0.0.1:16686`，按 service 选 `minicoding-server`，span 名见 §4.1（`session`/`turn`/`llm.chat_stream`/`tool.call`/`permission.check`）
4. **查 metrics**：Grafana `http://127.0.0.1:3000`（admin/admin，首次登录后修改），Explore → Prometheus

> 注意：当前版本 metrics 事件以 `tracing::debug!(target: "metrics::*")` 埋点（§3），trace 完整导出；metrics 的 OTLP 导出 layer 尚未启用，§7 基础设施中的 Prometheus/Grafana 已就绪，metrics 完整导出后即可消费。

### 7.5 安全与生产注意事项

- **仅本机绑定**：compose 默认所有端口绑定 `127.0.0.1`，trace 含路径/会话信息，勿改为 `0.0.0.0`
- **Jaeger 内存存储**：all-in-one 默认内存存储，重启丢数据；生产应换持久存储（Badger/ES/ClickHouse 等），见 Jaeger 文档
- **Grafana 密码**：默认 `admin/admin`，生产务必修改（`GRAFANA_ADMIN_PASSWORD`）
- **凭证安全**：minicoding 上报的 span 属性含会话 id/路径/模型名（§4.2），**不含 API key**（C-04 凭证不落日志/span）
- **镜像版本**：compose 中镜像版本经 `.env` 变量可覆盖（见 `.env.example`），升级时统一调整

### 7.6 本地日志文件（桌面端排查）

- **文件日志**：server/CLI 启动时除 stderr 外，额外写 `~/.minicoding/logs/server.log`（按天轮转，保留 7 份），stderr 与文件内容一致。文件层构建失败（home 不可解析/目录不可建）时降级为仅 stderr，不阻塞启动；
- **详细事件日志（`target` 无，字段前缀区分）**：
  - `llm.*`：`llm_call started`（provider/model/message_count，C-04 不含输入原文）与 `llm_call finished`（`elapsed_ms`/`input_tokens`/`output_tokens`/`cache_read_tokens`/`text_chars`/`reasoning_chars`/`tool_calls`/`stop_reason`）；
  - `tool.*`：`tool_call started/finished`（name/call_id/`elapsed_ms`/`is_error`/`output_bytes`），副作用工具另带 `permission.verdict`；
  - `turn.*`：`turn finished`（`elapsed_ms`/`outcome`），含超时/取消/重复工具循环终止；
  - 权限决策另有 `audit.log`（见 `docs/security.md`）；
- **前端**：Web 模式浏览器 devtools console 有 `[api]`（HTTP 请求）与 `[sse]`（事件类型，token/reasoning_delta 高频事件按 100 条计数合并）debug 日志；桌面 WebView 无 devtools，排查以 `server.log` 为准；
- **用法**：桌面端出问题（权限弹窗未出现、无结果等）时，直接看 `~/.minicoding/logs/server.log`（Windows 为 `%USERPROFILE%\.minicoding\logs\server.log`），按 `tool_call`/`llm_call`/`turn` 时间线定位。

---

## §8 实现路线图

### P0：Metrics 模块（最高优先级）

1. 创建 `core/metrics.rs`：定义所有 metrics 记录函数
2. 更新 `core/lib.rs`：导出 `metrics` 模块
3. 更新 `core/prelude`：re-export 关键 metrics 函数

### P1：补齐 13 处 span 断裂

按 crate 分批修复：
- **批次 A**（memory + journal）：4 + 1 = 5 处
- **批次 B**（storage + sandbox）：2 + 1 = 3 处
- **批次 C**（mcp + context + providers）：1 + 1 + 1 = 3 处
- **批次 D**（core config + event + tools）：1 + 1 + 1 = 3 处（注：部分已在 P1 批次中覆盖）

### P2：增强 span 属性

- `rt.rs`：llm_call / tool_call / permission span 补充属性
- `context/manager.rs`：compress span 补充 tokens_before/after

### P3：接入 metrics

- `rt.rs`：LLM 调用完成后 `record_llm_tokens` + `record_duration`
- `rt.rs`：工具调用完成后 `record_tool_call` + `record_duration`
- `rt.rs`：权限决策完成后 `record_permission`
- `context/manager.rs`：压缩完成后 `record_compress`
- `sandbox/`：熔断状态变更时 `set_circuit_breaker`

### P4：错误可观测性

- 在 `rt.rs` 的错误处理路径添加 `record_error`
- 在 `context/manager.rs` 压缩失败路径添加 `record_error`
- 在 `mcp/` 错误路径添加 `record_error`

---

## §9 验收标准

- [x] `core/metrics.rs` 创建，所有 metrics 函数有 doc comment（14 个函数）
- [x] 13 处 span 断裂全部补齐，`cargo clippy` 无警告
- [x] `rt.rs` 的 llm_call/tool_call/permission span 包含完整属性（含 `llm.provider`/`tool.side_effect`/`tool.parallel`/`permission.verdict`）
- [x] `context/manager.rs` 的 compress span 包含 `compress.tokens_before`/`compress.tokens_after`
- [x] 关键路径（llm_call/tool_call/permission/compress）接入 metrics
- [x] 错误路径接入 `metrics::record_error`（18 处调用点，9 类错误）
- [x] MCP/Hook/后台 shell 接入对应 metrics（`record_mcp_tool_call`/`set_mcp_connections`/`record_hook`/`set_background_shells`）
- [x] 活跃会话数与熔断状态 metrics 接入（`set_active_sessions`/`set_circuit_breaker`）
- [x] `cargo test --workspace` 全通过
- [x] `cargo clippy --workspace --all-features -- -D warnings` 全通过
- [x] 覆盖率不降低（≥80%）

---

## §10 与其他文档的引用关系

- 本文档扩展 `docs/design.md` §15（OTel 设计）
- span 名/属性常量定义在 `crates/minicoding-core/src/otel.rs`
- metrics 接口定义在 `crates/minicoding-core/src/metrics.rs`
- 审计日志设计见 `docs/security.md`
- 熔断约束见 `docs/rules.md` C-29（压缩熔断）/ C-30（沙箱拒绝熔断）
- crate 职责边界见 `docs/modules.md`
- 技术选型见 `docs/tech-stack.md` §9（tracing/OTel）
