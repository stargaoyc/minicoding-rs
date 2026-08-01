//! `OTel` 初始化入口（T-M0-4）。
//!
//! 桥接 `tracing` 与 OpenTelemetry：
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` 配置时安装 OTLP layer（`otel` feature 启用）；
//! - 未配置或 `otel` feature 未启用时降级为纯本地 fmt 日志。
//!
//! `otel` feature 未启用时本模块仍可编译，但 `init_with_otlp` 恒返回 `None`
//! （无 OTLP 导出能力）。

#[cfg(feature = "otel")]
use minicoding_core::otel::Sampler;

/// 初始化日志/trace。
///
/// `verbose` 控制 fmt 日志级别（`debug`/`warn`）。返回的 guard 用于
/// `otel` feature 启用且 OTLP 配置时优雅 flush；其他情况返回 `None`。
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

    // 未配置 OTLP 端点或 otel feature 未启用：纯 fmt 日志
    init_fmt_only(verbose);
    None
}

/// 本地 fmt 初始化（无 OTLP 导出）。
///
/// 仅安装 `tracing_subscriber::fmt` layer，输出到 stderr。
/// 重复调用安全（`try_init` 失败仅因重复初始化，吞掉错误）。
fn init_fmt_only(verbose: bool) {
    let filter = if verbose { "debug" } else { "warn" };
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
#[cfg(feature = "otel")]
fn init_with_otlp(
    verbose: bool,
) -> Result<Option<TracingGuard>, Box<dyn std::error::Error + Send + Sync>> {
    use minicoding_core::otel::SERVICE_NAME;
    use opentelemetry::KeyValue;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::trace::Sampler as SdkSampler;
    use tracing_opentelemetry::OpenTelemetryLayer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // 资源属性（§15.3）：service.name + service.version + host.name
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    let resource = Resource::builder()
        .with_attributes([
            KeyValue::new("service.name", SERVICE_NAME),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("host.name", hostname),
        ])
        .build();

    // 采样策略（§15.3）
    let sampler = match Sampler::from_env() {
        Sampler::AlwaysOn => SdkSampler::AlwaysOn,
        Sampler::TraceIdRatio(ratio) => SdkSampler::TraceIdRatioBased(ratio),
    };

    // OTLP HTTP exporter（默认协议，§15.3）。`http-proto` feature 决定协议。
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
    let tracer = provider.tracer("minicoding");
    let otel_layer = OpenTelemetryLayer::new(tracer);

    // fmt layer（本地日志，与 OTLP 并行）
    let filter = if verbose { "debug" } else { "warn" };
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(false);

    // 安装 subscriber（try_init 失败仅因重复初始化）
    let _ = tracing_subscriber::registry()
        .with(otel_layer)
        .with(fmt_layer)
        .with(
            tracing_subscriber::EnvFilter::try_new(filter)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();

    Ok(Some(TracingGuard {
        _inner: Some(TracingGuardInner { provider }),
    }))
}
