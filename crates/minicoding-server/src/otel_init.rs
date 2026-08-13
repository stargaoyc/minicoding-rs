//! `OTel` 初始化（server 入口，镜像 `minicoding-cli/src/otel_init.rs`）。
//!
//! 为什么复制而非复用 CLI 实现：依赖方向约束（AGENTS.md §3.2）禁止 `minicoding-server`
//! 依赖 `minicoding-cli`（边界 crate 不可被下层依赖）。两处实现需保持同步演进。
//!
//! 行为与 CLI 完全对齐：
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` 配置时安装 OTLP layer（`otel` feature 默认启用）；
//! - 未配置或初始化失败时降级为纯本地 fmt 日志，不阻塞启动（M0 验收标准）。
//!
//! `service.name` 使用 `minicoding-server`，与 CLI（`minicoding`）区分，便于
//! Jaeger/OTel 后端按入口过滤（见 `docs/observability.md` §7.3）。

#[cfg(feature = "otel")]
use minicoding_core::otel::Sampler;

/// `service.name` 资源属性值（server 入口）。
pub const SERVICE_NAME_SERVER: &str = "minicoding-server";

/// 初始化日志/trace。
///
/// 返回的 guard 在 `otel` feature 启用且 OTLP 配置时用于优雅 flush；其他情况返回 `None`。
#[must_use]
pub fn init_tracing(verbose: bool) -> Option<TracingGuard> {
    #[cfg(feature = "otel")]
    {
        if minicoding_core::otel::otlp_endpoint_configured() {
            return match init_with_otlp(verbose) {
                Ok(guard) => guard,
                Err(e) => {
                    // OTLP 初始化失败：降级 fmt，不阻塞启动
                    eprintln!("warn: OTLP 初始化失败，降级为本地 fmt 日志: {e}");
                    init_fmt_only(verbose);
                    None
                }
            };
        }
    }

    // 未配置 OTLP 端点：纯 fmt 日志
    init_fmt_only(verbose);
    None
}

/// 本地 fmt 初始化（无 OTLP 导出），输出到 stderr。
fn init_fmt_only(verbose: bool) {
    let filter = if verbose { "debug" } else { "info" };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Tracing guard：drop 时 flush OTLP exporter。
#[must_use = "drop guard flushes OTLP exporter"]
pub struct TracingGuard {
    #[cfg(feature = "otel")]
    _inner: Option<TracingGuardInner>,
}

#[cfg(feature = "otel")]
struct TracingGuardInner {
    provider: opentelemetry_sdk::trace::SdkTracerProvider,
}

#[cfg(feature = "otel")]
impl Drop for TracingGuardInner {
    fn drop(&mut self) {
        // 优雅关闭：flush 剩余 span 到 OTLP 后端
        if let Err(e) = self.provider.shutdown() {
            eprintln!("warn: OTLP shutdown 失败: {e:?}");
        }
    }
}

/// 安装 OTLP layer + fmt layer（`otel` feature 启用时）。
///
/// 实现与 `minicoding-cli/src/otel_init.rs` 的 `init_with_otlp` 保持一致（见模块注释）。
#[cfg(feature = "otel")]
fn init_with_otlp(
    verbose: bool,
) -> Result<Option<TracingGuard>, Box<dyn std::error::Error + Send + Sync>> {
    use opentelemetry::KeyValue;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::trace::Sampler as SdkSampler;
    use tracing_opentelemetry::OpenTelemetryLayer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // 资源属性（`design.md` §15.3）：service.name + service.version + host.name
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    let resource = Resource::builder()
        .with_attributes([
            KeyValue::new("service.name", SERVICE_NAME_SERVER),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("host.name", hostname),
        ])
        .build();

    // 采样策略（`design.md` §15.3）
    let sampler = match Sampler::from_env() {
        Sampler::AlwaysOn => SdkSampler::AlwaysOn,
        Sampler::TraceIdRatio(ratio) => SdkSampler::TraceIdRatioBased(ratio),
    };

    // OTLP HTTP exporter（默认协议）
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
        .map_err(|e| format!("OTLP exporter build 失败: {e:?}"))?;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_resource(resource)
        .build();

    // 通过 `TracerProvider` trait 方法获取 tracer，桥接到 tracing
    let tracer = provider.tracer("minicoding-server");
    let otel_layer = OpenTelemetryLayer::new(tracer);

    // fmt layer（本地日志，与 OTLP 并行）
    let filter = if verbose { "debug" } else { "info" };
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(false);

    // 安装 subscriber（try_init 失败仅因重复初始化）
    let _ = tracing_subscriber::registry()
        .with(otel_layer)
        .with(fmt_layer)
        .with(
            tracing_subscriber::EnvFilter::try_new(filter)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    Ok(Some(TracingGuard {
        _inner: Some(TracingGuardInner { provider }),
    }))
}
