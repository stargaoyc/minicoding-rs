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
use minicoding_server::{ServerConfig, serve};

/// minicoding-server — HTTP/SSE server
#[derive(Parser, Debug)]
#[command(name = "minicoding-server", version, about, long_about = None)]
struct Cli {
    /// 监听地址
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: String,

    /// LLM provider 类型（`openai`/`anthropic`/`ollama`）
    #[arg(long, env = "OPENAI_PROVIDER", default_value = "openai")]
    provider: String,

    /// API base URL
    #[arg(long, env = "OPENAI_API_BASE")]
    api_base: Option<String>,

    /// API key
    #[arg(long, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    /// 模型名称
    #[arg(long, env = "OPENAI_MODEL", default_value = "gpt-4o")]
    model: String,

    /// 工作目录
    #[arg(long, default_value = ".")]
    workdir: String,

    /// 系统 prompt 覆盖
    #[arg(long)]
    system: Option<String>,

    /// 权限交互超时（秒）
    #[arg(long, default_value_t = 300)]
    permission_timeout_sec: u64,

    /// 启用详细日志
    #[arg(long, short = 'v')]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // 初始化日志
    let _guard = init_tracing(cli.verbose);

    let bind: std::net::SocketAddr = cli
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address `{}`: {e}", cli.bind))?;

    // 默认 API base（按 provider 选择默认值）
    let api_base = cli.api_base.unwrap_or_else(|| match cli.provider.as_str() {
        "ollama" => "http://localhost:11434".to_string(),
        "anthropic" => "https://api.anthropic.com".to_string(),
        _ => "https://api.openai.com".to_string(),
    });

    let api_key = cli.api_key.unwrap_or_default();

    let cfg = ServerConfig {
        bind,
        provider_kind: cli.provider,
        api_base,
        api_key,
        model: cli.model,
        workdir: Utf8PathBuf::from(cli.workdir),
        system: cli.system,
        permission_timeout_sec: cli.permission_timeout_sec,
    };

    serve(cfg).await
}

/// 初始化 tracing 日志（简化版，server 不需要 OTLP）。
fn init_tracing(verbose: bool) -> tracing::subscriber::DefaultGuard {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::util::SubscriberInitExt;
    let filter = if verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .set_default()
}
