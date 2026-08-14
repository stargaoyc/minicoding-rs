//! `OTel` 初始化（server 入口，镜像 `minicoding-cli/src/otel_init.rs`）。
//!
//! 为什么复制而非复用 CLI 实现：依赖方向约束（AGENTS.md §3.2）禁止 `minicoding-server`
//! 依赖 `minicoding-cli`（边界 crate 不可被下层依赖）。两处实现需保持同步演进。
//!
//! 行为与 CLI 对齐（差异：**文件日志双写**，桌面端排查专用）：
//! - 所有启动路径（OTLP 或本地）均额外写 `~/.minicoding/logs/server.log`（按天轮转，
//!   保留 7 份），stderr 与文件内容一致；
//! - 文件层安装失败（无法解析 home/创建目录）降级为仅 stderr，不阻塞启动（M0 验收）；
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` 配置时安装 OTLP layer（`otel` feature 默认启用）；
//! - 未配置或初始化失败时降级为纯本地 fmt 日志，不阻塞启动。
//!
//! `service.name` 使用 `minicoding-server`，与 CLI（`minicoding`）区分，便于
//! Jaeger/OTel 后端按入口过滤（见 `docs/observability.md` §7.3）。

#[cfg(feature = "otel")]
use minicoding_core::otel::Sampler;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// `service.name` 资源属性值（server 入口）。
pub const SERVICE_NAME_SERVER: &str = "minicoding-server";

/// 初始化日志/trace。
///
/// 返回的 guard 在 `otel` feature 启用且 OTLP 配置时用于优雅 flush；其他情况用于保活
/// 文件日志 `non_blocking` worker。guard drop 后对应日志通道可能丢失。
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
                    Some(init_fmt_only(verbose))
                }
            };
        }
    }

    // 未配置 OTLP 端点：纯 fmt 日志
    Some(init_fmt_only(verbose))
}

/// 本地 fmt 初始化（无 OTLP 导出）：stderr + 文件双写。
///
/// 文件层构建失败也不阻塞启动（降级为仅 stderr）。
fn init_fmt_only(verbose: bool) -> TracingGuard {
    let filter = if verbose { "debug" } else { "info" };
    let env_filter = tracing_subscriber::EnvFilter::try_new(filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let fmt_layer = tracing_subscriber::fmt::layer().with_target(false);

    // 文件层：`~/.minicoding/logs/server.log`（按天轮转，保留 7 份）。类型全部交由
    // 推断——实测显式标注或 `impl Trait` 返回在 tracing-subscriber 0.3.23 会触发
    // `Layer` trait 不满足（泛型参数与推断结果不一致），故内联此处（两处保持同步）。
    let (file_layer, file_guard) = match minicoding_core::paths::minicoding_home() {
        Ok(home) if std::fs::create_dir_all(home.join("logs")).is_ok() => {
            let log_dir = home.join("logs");
            let appender = RollingFileAppender::new(Rotation::DAILY, log_dir, "server.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_target(false)
                        .with_ansi(false)
                        .with_writer(writer),
                ),
                Some(guard),
            )
        }
        _ => {
            eprintln!("warn: 无法创建日志目录，跳过文件日志（仅 stderr）");
            (None, None)
        }
    };

    // 文件层为 `None`（构建失败）时只装 stderr 层；`EnvFilter` 作为全局过滤器挂载尾部
    // （`fmt::Layer` 无 `with_env_filter` 方法，与 `init_with_otlp` 同一模式）；
    // `try_init` 失败仅因重复初始化。
    let _ = match file_layer {
        Some(layer) => tracing_subscriber::registry()
            .with(fmt_layer)
            .with(layer)
            .with(env_filter)
            .try_init(),
        None => tracing_subscriber::registry()
            .with(fmt_layer)
            .with(env_filter)
            .try_init(),
    };
    TracingGuard {
        #[cfg(feature = "otel")]
        _inner: None,
        _file: file_guard,
    }
}

/// Tracing guard：drop 时 flush OTLP exporter 并释放文件日志 worker。
#[must_use = "drop guard flushes OTLP exporter / 文件日志 worker"]
pub struct TracingGuard {
    #[cfg(feature = "otel")]
    _inner: Option<TracingGuardInner>,
    /// 文件日志 `non_blocking` worker：guard drop 后队列中的日志可能丢失。
    _file: Option<WorkerGuard>,
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

    // fmt layer（本地日志，与 OTLP 并行）+ 文件 layer（与 fmt 同 filter）
    let filter = if verbose { "debug" } else { "info" };
    let env_filter = tracing_subscriber::EnvFilter::try_new(filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(false);

    // 文件层（与 init_fmt_only 同一段，见上；类型须推断，不可显式标注）
    let (file_layer, file_guard) = match minicoding_core::paths::minicoding_home() {
        Ok(home) if std::fs::create_dir_all(home.join("logs")).is_ok() => {
            let log_dir = home.join("logs");
            let appender = RollingFileAppender::new(Rotation::DAILY, log_dir, "server.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_target(false)
                        .with_ansi(false)
                        .with_writer(writer),
                ),
                Some(guard),
            )
        }
        _ => {
            eprintln!("warn: 无法创建日志目录，跳过文件日志（仅 stderr）");
            (None, None)
        }
    };

    // 安装 subscriber（try_init 失败仅因重复初始化）
    let _ = match file_layer {
        Some(layer) => tracing_subscriber::registry()
            .with(otel_layer)
            .with(fmt_layer)
            .with(layer)
            .with(env_filter)
            .try_init(),
        None => tracing_subscriber::registry()
            .with(otel_layer)
            .with(fmt_layer)
            .with(env_filter)
            .try_init(),
    };

    Ok(Some(TracingGuard {
        _inner: Some(TracingGuardInner { provider }),
        _file: file_guard,
    }))
}
