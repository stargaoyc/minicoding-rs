//! M-12 集成测试：`tools.parallel_reads` 并行度语义（0=串行；>0=有界并发）
//! + turn 边界白名单热更新（config.toml 中 `[tools] parallel_reads` 当轮生效）。

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use camino::Utf8PathBuf;
use minicoding_core::config::RuntimeConfig;
use minicoding_core::model::{
    SideEffect, StopReason, ToolError, ToolResult, ToolSchema, UserInput,
};
use minicoding_core::provider::{BoxFuture, Delta, ToolCallDelta};
use minicoding_core::runtime::{Runtime, RuntimeBuilder};
use minicoding_core::tool::{Tool, ToolContext, ToolRegistry};

use common::{InMemoryStorage, ScriptedProvider, TestContext, text_deltas};

/// 并发探测工具：execute 期间自增 active、记录峰值，sleep 后自减。
///
/// 用于观测只读桶的实际并行度——`MockTool` 立即返回无法观测并发，
/// 而本工具用 30ms sleep 拉开时间窗，让并行的调用能同时处于 active。
struct ProbeTool {
    name: &'static str,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Tool for ProbeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            name: String::new(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        })
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let active = self.active.clone();
        let peak = self.peak.clone();
        Box::pin(async move {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolResult::ok_text("probe done"))
        })
    }
}

/// 注册 3 个只读 probe 工具 + 单条多工具调用脚本，构造 Runtime。
fn build_runtime(parallel_reads: u32, base: RuntimeConfig) -> (Runtime, Arc<AtomicUsize>) {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    for name in ["g0", "g1", "g2"] {
        tools.register(Arc::new(ProbeTool {
            name,
            active: active.clone(),
            peak: peak.clone(),
        }));
    }

    let provider = ScriptedProvider::new(vec![
        vec![
            Delta::ToolCall(ToolCallDelta {
                index: 0,
                id: Some("c0".into()),
                name: Some("g0".into()),
                args_chunk: Some("{}".into()),
            }),
            Delta::ToolCall(ToolCallDelta {
                index: 1,
                id: Some("c1".into()),
                name: Some("g1".into()),
                args_chunk: Some("{}".into()),
            }),
            Delta::ToolCall(ToolCallDelta {
                index: 2,
                id: Some("c2".into()),
                name: Some("g2".into()),
                args_chunk: Some("{}".into()),
            }),
            Delta::Stop(StopReason::ToolUse),
        ],
        text_deltas("done"),
    ]);

    let mut config = base;
    config.tools.parallel_reads = parallel_reads;
    let rt = RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(TestContext::new("test")))
        .storage(Arc::new(InMemoryStorage::new()))
        .tools(tools)
        .config(config)
        .workdir(Utf8PathBuf::from("."))
        .build()
        .expect("runtime build");
    (rt, peak)
}

/// 验收 1（design.md M-12）：`parallel_reads = 0` → 只读工具串行执行。
// F1：start_paused 虚拟时钟——ProbeTool 的 30ms 时间窗即时推进，
// 并发重叠语义不变（并行桶内 3 个调用 park 于同一线，唤醒时 peak 观测不变）
#[tokio::test(start_paused = true)]
async fn parallel_reads_0_serial() {
    let (rt, peak) = build_runtime(0, RuntimeConfig::default());
    rt.run_turn(UserInput::from_text("run three"))
        .await
        .expect("turn ok");
    assert_eq!(peak.load(Ordering::SeqCst), 1, "串行模式下并发峰值应为 1");
}

/// 验收 2：`parallel_reads = 4` → 只读工具并行，且并发不超过配置上限
/// （3 个调用全部并行 → 峰值 3，且 ≤ 4）。
// F1：start_paused 虚拟时钟——ProbeTool 的 30ms 时间窗即时推进，
// 并发重叠语义不变（并行桶内 3 个调用 park 于同一线，唤醒时 peak 观测不变）
#[tokio::test(start_paused = true)]
async fn parallel_reads_4_bounds_concurrency() {
    let (rt, peak) = build_runtime(4, RuntimeConfig::default());
    rt.run_turn(UserInput::from_text("run three"))
        .await
        .expect("turn ok");
    let p = peak.load(Ordering::SeqCst);
    assert!(
        (2..=4).contains(&p),
        "3 个只读调用应并行、且并发不超过 4，实际峰值 {p}"
    );
}

/// 验收 3：turn 边界白名单热更新——config.toml 显式声明 `parallel_reads = 0` 时，
/// 即使 Runtime 构造配置为默认 8，首轮 `run_turn` 即生效为串行。
///
/// `Event::ConfigChanged` 通知到达本身已有 watcher 单测覆盖，此处验证
/// `reload_safe_config` 在真实 turn 边界应用白名单字段（best-effort 读文件）。
// F1：start_paused 虚拟时钟——ProbeTool 的 30ms 时间窗即时推进，
// 并发重叠语义不变（并行桶内 3 个调用 park 于同一线，唤醒时 peak 观测不变）
#[tokio::test(start_paused = true)]
async fn turn_boundary_whitelist_applies_parallel_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    // 仅声明 `[tools] parallel_reads = 0`（白名单 presence 判断：显式存在才应用）
    std::fs::write(&config_path, "[tools]\nparallel_reads = 0\n").expect("write config");

    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    for name in ["g0", "g1", "g2"] {
        tools.register(Arc::new(ProbeTool {
            name,
            active: active.clone(),
            peak: peak.clone(),
        }));
    }
    let provider = ScriptedProvider::new(vec![
        vec![
            Delta::ToolCall(ToolCallDelta {
                index: 0,
                id: Some("c0".into()),
                name: Some("g0".into()),
                args_chunk: Some("{}".into()),
            }),
            Delta::ToolCall(ToolCallDelta {
                index: 1,
                id: Some("c1".into()),
                name: Some("g1".into()),
                args_chunk: Some("{}".into()),
            }),
            Delta::ToolCall(ToolCallDelta {
                index: 2,
                id: Some("c2".into()),
                name: Some("g2".into()),
                args_chunk: Some("{}".into()),
            }),
            Delta::Stop(StopReason::ToolUse),
        ],
        text_deltas("done"),
    ]);
    let config_path_utf8 =
        Utf8PathBuf::from_path_buf(config_path).expect("tempdir path must be valid UTF-8");
    // 构造配置默认 parallel_reads = 8，但注入 config_path
    let rt = RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(TestContext::new("test")))
        .storage(Arc::new(InMemoryStorage::new()))
        .tools(tools)
        .config(RuntimeConfig::default())
        .with_config_path(config_path_utf8)
        .workdir(Utf8PathBuf::from("."))
        .build()
        .expect("runtime build");

    rt.run_turn(UserInput::from_text("run three"))
        .await
        .expect("turn ok");
    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "turn 边界应应用文件中的 parallel_reads=0（串行）"
    );
}

/// 记录执行顺序的工具（CORE-15，2026-08-25 R2 审查）：把自身名字推入共享
/// 序列。`side_effect` 可配置以构造"副作用在前、只读在后"的乱序场景。
struct OrderProbeTool {
    name: &'static str,
    side_effect: SideEffect,
    order: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Tool for OrderProbeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            name: String::new(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        })
    }
    fn side_effect(&self) -> SideEffect {
        self.side_effect
    }
    fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
        let order = self.order.clone();
        let name = self.name.to_string();
        Box::pin(async move {
            order.lock().expect("order lock").push(name);
            Ok(ToolResult::ok_text("done"))
        })
    }
}

/// 验收 4（A-P1 保序回退，CORE-15）：LLM 原始顺序为 [写, 读] 时，
/// "先并行读再串行写"的常规调度会颠倒语义——必须回退为**全串行且按原始
/// 顺序**执行。锁定 read-after-write 的顺序不变式。
#[tokio::test]
async fn mixed_order_falls_back_to_serial_in_llm_order() {
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(OrderProbeTool {
        name: "w",
        side_effect: SideEffect::FileWrite,
        order: order.clone(),
    }));
    tools.register(Arc::new(OrderProbeTool {
        name: "r",
        side_effect: SideEffect::None,
        order: order.clone(),
    }));

    let provider = ScriptedProvider::new(vec![
        vec![
            // 原始顺序：先写后读（读在副作用之后 → 禁用并行桶）
            Delta::ToolCall(ToolCallDelta {
                index: 0,
                id: Some("cw".into()),
                name: Some("w".into()),
                args_chunk: Some("{}".into()),
            }),
            Delta::ToolCall(ToolCallDelta {
                index: 1,
                id: Some("cr".into()),
                name: Some("r".into()),
                args_chunk: Some("{}".into()),
            }),
            Delta::Stop(StopReason::ToolUse),
        ],
        text_deltas("done"),
    ]);

    let rt = RuntimeBuilder::new()
        .provider(Arc::new(provider))
        .context(Arc::new(TestContext::new("test")))
        .storage(Arc::new(InMemoryStorage::new()))
        .tools(tools)
        .config(RuntimeConfig::default())
        .workdir(Utf8PathBuf::from("."))
        .build()
        .expect("runtime build");

    rt.run_turn(UserInput::from_text("write then read"))
        .await
        .expect("turn ok");

    let seq = order.lock().expect("order lock").clone();
    assert_eq!(
        seq,
        vec!["w".to_string(), "r".to_string()],
        "写在前读在后时必须按 LLM 原始顺序全串行执行（read-after-write 保序）"
    );
}
