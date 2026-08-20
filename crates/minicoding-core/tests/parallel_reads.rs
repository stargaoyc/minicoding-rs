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
#[tokio::test]
async fn parallel_reads_0_serial() {
    let (rt, peak) = build_runtime(0, RuntimeConfig::default());
    rt.run_turn(UserInput::from_text("run three"))
        .await
        .expect("turn ok");
    assert_eq!(peak.load(Ordering::SeqCst), 1, "串行模式下并发峰值应为 1");
}

/// 验收 2：`parallel_reads = 4` → 只读工具并行，且并发不超过配置上限
/// （3 个调用全部并行 → 峰值 3，且 ≤ 4）。
#[tokio::test]
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
#[tokio::test]
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
