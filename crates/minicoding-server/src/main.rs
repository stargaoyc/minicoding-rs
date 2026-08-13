//! # minicoding-server（bin）
//!
//! HTTP/SSE server 二进制入口。可独立运行，也可通过 `minicoding serve` 子命令启动。
//!
//! ```text
//! minicoding-server --bind 127.0.0.1:8080
//! minicoding serve --port 8080
//! ```

#![deny(clippy::all, clippy::pedantic)]

use anyhow::Result;
use camino::Utf8PathBuf;
use clap::Parser;
use minicoding_server::otel_init::{TracingGuard, init_tracing};
use minicoding_server::{ServerConfig, serve};

/// minicoding-server — HTTP/SSE server
#[derive(Parser, Debug)]
#[command(name = "minicoding-server", version, about, long_about = None)]
struct Cli {
    /// 监听地址
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: String,

    /// LLM provider 类型（`openai`/`anthropic`/`ollama`，默认从 `config.toml` 读取）
    #[arg(long, env = "OPENAI_PROVIDER")]
    provider: Option<String>,

    /// Provider 自定义显示名（用于日志/metrics，不影响协议分派，与 CLI `--provider-name` 对齐）
    #[arg(long, env = "MINICODING_PROVIDER_NAME")]
    provider_name: Option<String>,

    /// API base URL
    #[arg(long, env = "OPENAI_API_BASE")]
    api_base: Option<String>,

    /// API key
    #[arg(long, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    /// 模型名称（默认从 `config.toml` 读取）
    #[arg(long, env = "OPENAI_MODEL")]
    model: Option<String>,

    /// 工作目录
    #[arg(long, default_value = ".")]
    workdir: String,

    /// 系统 prompt 覆盖
    #[arg(long)]
    system: Option<String>,

    /// 权限交互超时（秒）
    #[arg(long, default_value_t = 300)]
    permission_timeout_sec: u64,

    /// 静态资源目录（M9 `--web`，托管前端 SPA，见 `design.md` §26.7）
    #[arg(long)]
    web: Option<String>,

    /// CORS 允许的来源（M9，可多次指定；默认允许任意来源，见 `design.md` §26.6）
    #[arg(long = "cors-origin")]
    cors_origins: Vec<String>,

    /// 启用详细日志
    #[arg(long, short = 'v')]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // 初始化日志/trace（配置 `OTEL_EXPORTER_OTLP_ENDPOINT` 时接入 OTLP，见 `otel_init`）
    let _otel_guard: Option<TracingGuard> = init_tracing(cli.verbose);

    let bind: std::net::SocketAddr = cli
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address `{}`: {e}", cli.bind))?;

    // 解析 provider 配置（CLI > env > config.toml > 默认，与 `minicoding serve` 一致）
    let file_provider = minicoding_core::config::load_config()
        .map(|c| c.provider)
        .unwrap_or_default();

    let provider_kind = cli
        .provider
        .unwrap_or_else(|| file_provider.default.clone());
    let provider_name = cli.provider_name.or(file_provider.name.clone());
    let api_key = cli.api_key.unwrap_or_else(|| file_provider.api_key.clone());
    let model = cli.model.unwrap_or_else(|| file_provider.model.clone());

    // api_base：CLI > config.toml > 按 provider 选默认
    let api_base = cli.api_base.unwrap_or_else(|| {
        if file_provider.api_base.is_empty() {
            match provider_kind.as_str() {
                "ollama" => "http://localhost:11434".to_string(),
                "anthropic" => "https://api.anthropic.com".to_string(),
                _ => "https://api.openai.com".to_string(),
            }
        } else {
            file_provider.api_base.clone()
        }
    });

    let cfg = ServerConfig {
        bind,
        provider_kind,
        provider_name,
        api_base,
        api_key,
        model,
        workdir: Utf8PathBuf::from(cli.workdir),
        system: cli.system,
        permission_timeout_sec: cli.permission_timeout_sec,
        web_dir: cli.web.map(Utf8PathBuf::from),
        cors_origins: cli.cors_origins,
    };

    serve(cfg).await
}
