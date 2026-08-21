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

    /// 安全预设：`auto`（默认，工作区可写其余 Ask）/ `read-only` /
    /// `external-sandbox`（外部沙箱）/ `full-access`（沙箱外全自动，
    /// 仅受信隔离容器内使用，C-22 red 警告）
    #[arg(long, default_value = "auto")]
    preset: String,

    /// API 鉴权 token（S1）。省略时自动生成并以 `SERVER_TOKEN=<t>` 打印到 stdout。
    #[arg(long)]
    auth_token: Option<String>,

    /// 关闭 API 鉴权（仅限本机隔离环境；任意进程可完全控制 Agent）
    #[arg(long, default_value_t = false)]
    no_auth: bool,
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
    let file_config = minicoding_core::config::load_config().unwrap_or_default();
    let file_provider = &file_config.provider;

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

    // S1：鉴权 token——显式指定 > 自动生成（打印 SERVER_TOKEN=）> --no-auth 关闭
    let auth_token = if cli.no_auth {
        eprintln!(
            "WARNING: API 鉴权已禁用（--no-auth）：本机任意进程可读取会话、代答权限、执行命令"
        );
        None
    } else {
        Some(
            cli.auth_token
                .unwrap_or_else(minicoding_server::generate_auth_token),
        )
    };
    if let Some(t) = &auth_token {
        println!("SERVER_TOKEN={t}");
    }

    let cfg = ServerConfig {
        bind,
        auth_token,
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
        preset: cli.preset,
        // 模型参数/上下文默认值：config.toml > 内置默认（`RuntimeConfig::default()`）
        timeout_sec: file_provider.timeout_sec,
        max_retries: file_provider.max_retries,
        small_model: file_provider.small.as_ref().map(|s| s.model.clone()),
        turn_timeout_sec: file_config.context.turn_timeout_sec,
        compress: file_config.context.compress,
    };

    serve(cfg).await
}
