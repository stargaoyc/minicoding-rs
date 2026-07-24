# 技术选型说明

本文记录 `minicoding-rs` 的关键技术依赖、选型理由与备选方案。所有选型遵循三条原则：

1. **优先成熟、社区活跃的 crate**，避免维护风险；
2. **异步优先**，统一基于 `tokio` 运行时；
3. **最小依赖**，能不引入就不引入，必要时自行实现薄封装。

---

## 1. 语言与编译工具链

| 项 | 选择 | 说明 |
|----|------|------|
| 语言 | Rust (edition 2024) | 性能、内存安全、零成本抽象；适合构建长驻 CLI/Agent |
| MSRV | 1.85+ | edition 2024 稳定门槛（1.85 正式包含 edition 2024 与稳定的 `async fn in trait`） |
| 工具链 | `cargo` + `rustfmt` + `clippy` | 标准组合 |
| Workspace | Cargo Workspace + 多 crate | 见 `modules.md` |

### 1.1 为何选 Rust

- **启动快**：编译为原生二进制，冷启动远优于 Node/Python 实现，适合 CLI。
- **内存安全**：Agent 会执行文件/命令等高权限操作，内存安全降低攻击面。
- **并发模型**：`tokio` 提供成熟的异步 IO，便于流式响应与并行工具调用。
- **可嵌入**：library crate 可被其他 Rust 项目直接依赖，无运行时依赖。

---

## 2. 异步运行时与并发

| 用途 | crate | 版本 | 备选 | 理由 |
|------|-------|------|------|------|
| 运行时 | `tokio` | 1.x | `async-std` | 生态最广，HTTP/IO 生态默认依赖 |
| 异步 trait | 原生 `async fn in trait` + `trait-variant` | - | `async-trait` | edition 2024 + MSRV 1.85 后原生稳定；`trait-variant` 生成 Send 变体使 trait 可作 `dyn` 对象（Runtime 需 `Arc<dyn Trait>`） |
| 并发原语 | `tokio::sync` | - | `futures` | 与运行时一致 |
| 通道 | `tokio::sync::mpsc` / `broadcast` | - | `flume` | 满足流式与事件广播需求 |

> **决策**：避免同时引入 `async-std` 与 `tokio`，全栈统一 `tokio`。

---

## 3. HTTP 与 LLM 客户端

| 用途 | crate | 理由 |
|------|-------|------|
| HTTP 客户端 | `reqwest` (rustls-tls) | 事实标准；使用 `rustls` 避免 OpenSSL 系统依赖，便于跨平台静态编译 |
| SSE 解析 | `eventsource-stream` / 自实现 | 流式响应需要 Server-Sent Events 增量解析 |
| JSON | `serde` + `serde_json` | 标配 |
| 反序列化错误容忍 | `serde_json` + 自定义 visitor | LLM 返回 JSON 可能不规范，需要宽松解析 |
| WebSocket（可选） | `tokio-tungstenite` | 部分本地模型通过 WS 交互 |

### 3.1 为什么不直接用各 Provider 官方 SDK

- Rust 生态下官方 SDK 缺失或维护弱；
- 多 Provider 需要统一抽象，自实现薄客户端更可控；
- 避免引入多余依赖与版本耦合。

---

## 4. CLI 与 TUI

| 用途 | crate | 备选 | 理由 |
|------|-------|------|------|
| 参数解析 | `clap` (derive) | `argh` | 功能完整、文档好；derive 风格与项目一致 |
| 进度与交互 | `indicatif` | - | 流式 token 输出与 spinner |
| 彩色输出 | `anstream` + `anstyle` | `colored` | 与 `clap` 生态对齐，支持非 TTY 降级 |
| 行编辑 | `rustyline` | `reedline` | 成熟、依赖少；后续 TUI 阶段评估 `reedline` |
| TUI（后续） | `ratatui` + `crossterm` | - | 现代化 TUI 框架，跨平台 |

---

## 5. 文件与代码处理

| 用途 | crate | 理由 |
|------|-------|------|
| 文件系统 | `std::fs` + `tokio::fs` | 够用即可 |
| 路径处理 | `std::path` + `camino` | `camino` 提供 `Utf8PathBuf`，避免 OS 字符集边界问题 |
| Glob 匹配 | `globset` | `ripgrep` 同源，性能强 |
| 正则 | `regex` | 标配；不支持回溯但够用 |
| 目录遍历 | `ignore` | 尊重 `.gitignore`，与 `ripgrep` 行为一致 |
| 文件监听（可选） | `notify` | 用于 watch 模式 |

---

## 6. Token 计数与上下文

| 用途 | crate / 方案 | 理由 |
|------|--------------|------|
| BPE 分词 | `tiktoken-rs` | OpenAI 模型 token 计数；离线、纯 Rust |
| Claude 计数 | 官方计数接口 / 启发式估算 | Anthropic token 计算需走接口或近似 |
| 通用估算 | `tiktoken-rs` cl100k 近似 | 兜底策略，误差可接受 |

> **决策**：抽象 `Tokenizer` trait，按 provider/model 选择实现；找不到精确分词器时使用 `cl100k` 近似并打标。

---

## 7. 日志、追踪与可观测性

OpenTelemetry 是**一等公民**（非后续可选），从 M0 起接入。业务代码只写 `tracing` 宏，subscriber 层同时输出本地文件日志与 OTLP trace，无重复埋点。

| 用途 | crate | 理由 |
|------|-------|------|
| 日志门面 | `tracing` | 结构化日志 + span，trace 与日志同源 |
| 日志输出 | `tracing-subscriber` | 标配，多层 layer（fmt + otel） |
| 文件日志 | `tracing-appender` | 滚动日志，本地排障 |
| OTel 桥接 | `tracing-opentelemetry` | 把 `tracing` span 导出为 OTel span |
| OTel SDK | `opentelemetry` + `opentelemetry-otlp` + `opentelemetry_sdk` | OTLP/HTTP+gRPC 导出，对接 Jaeger/Tempo/Grafana |
| 采样 | `opentelemetry_sdk::Sampler` | `AlwaysOn`（调试）/ `TraceIdRatio`（生产） |

> **决策**：所有跨组件边界（session/turn/llm_call/tool_call/compress/permission）必须打 OTel span，关键属性见 `design.md` §15.2。后端地址由标准环境变量 `OTEL_EXPORTER_OTLP_ENDPOINT` / `OTEL_TRACES_SAMPLER` 控制，零代码改动即可切换。

---

## 8. 配置与序列化

| 用途 | crate | 理由 |
|------|-------|------|
| 配置文件 | `toml` + `serde` | `Cargo.toml` 同源，Rust 生态首选 |
| 配置加载 | 自实现 layered config | 项目 / 用户 / 环境变量 / CLI 参数分层合并 |
| 环境变量 | `std::env` + `figment`（评估） | 简单场景手写，复杂场景用 `figment` |
| 时间 | `time` | 比 `chrono` 更轻、更现代 |
| UUID | `uuid` | 会话/消息 ID |

---

## 9. 错误处理

| 用途 | crate | 理由 |
|------|-------|------|
| 错误类型 | `thiserror` | library crate 错误定义 |
| 错误传播 | `anyhow` | 应用层（CLI）错误聚合 |
| 不可达断言 | `unreachable!` / `panic` | 程序员错误直接 panic |

> **约定**：library crate（`minicoding-core` 等）只返回 `thiserror` 定义的具体错误；`minicoding-cli` 在边界用 `anyhow::Result` 聚合并格式化输出。

---

## 10. 测试与质量

| 用途 | crate | 理由 |
|------|-------|------|
| 单测/集成测试 | `cargo test` | 标配 |
| 异步测试 | `tokio::test` | 标配 |
| HTTP mock | `wiremock` / `httpmock` | Provider 客户端测试 |
| 临时文件 | `tempfile` | 文件工具测试 |
| 快照测试 | `insta` | 配置 schema、CLI 输出快照 |
| 覆盖率 | `cargo-llvm-cov` | 基于 LLVM，准确 |
| 模糊测试（可选） | `cargo-fuzz` | 解析器（SSE/JSON）模糊测试 |

---

## 11. 安全相关（沙箱为一等公民，见 `security.md` §8）

OS 级沙箱升级为一等公民后，安全相关依赖按"应用层 + 内核级"两道防线组织。沙箱驱动集中在 `minicoding-sandbox` crate，平台 C 绑定不污染 core。

| 用途 | crate / 方案 | 平台 | 理由 |
|------|--------------|:---:|------|
| TLS | `rustls` | 全平台 | 避免系统 OpenSSL，便于静态编译 |
| 凭证存储 | OS keychain（`keyring`） / 文件 0600 | 全平台 | 不把密钥写进配置明文 |
| 应用层路径沙箱 | `std::path::canonicalize` + `camino` | 全平台 | 防目录穿越（第一道防线，`security.md` §3） |
| Linux 文件系统沙箱 | `landlock` | Linux 5.13+ | 内核 LSM，限制可写范围；纯 Rust 绑定，无 C 依赖 |
| Linux 系统调用过滤 | `libseccomp` | Linux | seccomp-bpf 白名单系统调用（禁 `ptrace`/`mount`/`reboot`/`kexec_load`） |
| macOS 沙箱 | `sandbox-exec`（系统自带）+ 自生成 profile | macOS 12+ | Seatbelt 框架，无需额外 crate；通过 `tokio::process` 调用 `/usr/bin/sandbox-exec -p` |
| Windows 受限令牌 | `windows` crate | Windows 10+ | 受限 token + Job Object + DACL 限制写路径；成熟度低于 macOS/Linux，初期可降级 |
| 进程硬化 | `libc`（`PR_SET_DUMPABLE`/`RLIMIT_CORE`） | Linux/Unix | pre-main 禁 ptrace/core dump，清 `LD_*`/`DYLD_*` |
| 跨进程文件锁 | `fs2` | 全平台 | 会话文件互斥（`data-model.md` §10） |
| 文件权限收紧 | `std::fs` + `cfg!(unix)` `chmod 0600/0700` | Unix | `~/.minicoding/` 与会话文件权限收紧 |

> **平台检测策略**：`minicoding-sandbox::detect_driver()` 在编译期按 `cfg!(target_os)` 选实现。无可用硬隔离时（如 Windows 早期版本、不支持 Landlock 的旧内核）返回 `NoopDriver`（来自 core）并打 `warn`，依赖容器自身隔离（对应 `ExternalSandbox` 策略）。`landlock` 与 `libseccomp` 通过 cargo `[target.'cfg(target_os = "linux")'.dependencies]` 条件引入，非 Linux 平台不编译。

### 11.1 Hooks 与 MCP 相关

| 用途 | crate / 方案 | 理由 |
|------|--------------|------|
| Hooks 脚本执行 | `tokio::process::Command` | Hooks 以"外部可执行 + JSON over stdio"为主协议（见 `hooks.md` §3），无需额外 crate |
| Hook JSON 协议 | `serde_json` | stdin/stdout 单行 JSON，复用既有依赖 |
| MCP 客户端（早期） | 自实现 stdio 薄封装 | M4 先交付 `stdio_only` 客户端，仅依赖 `tokio::process` + `serde_json` |
| MCP 客户端（完整） | `rmcp` | 官方 Rust MCP SDK，支持 stdio + http + OAuth；M5+ 引入替换薄封装 |
| MCP server 暴露 | `rmcp` | `minicoding serve --as-mcp-server` 把内置工具暴露给其他 Agent（阶段 7） |
| 工具检索（阶段 6+） | 自实现 BM25 | MCP 工具多时按需检索（见 `design.md` §19.6），不引入向量依赖 |

> **依赖隔离**：`rmcp` 含完整网络栈与 OAuth 流程，仅在 `minicoding-mcp` crate 引入；`minicoding-core` 仅定义 `McpClient` trait，不依赖 `rmcp`，保持 core 轻量。Hooks 无新增依赖，复用 `tokio::process`。

---

## 12. 依赖治理策略

- **审计**：`cargo audit` + `cargo deny` 接入 CI。
- **版本锁定**：`Cargo.lock` 提交到仓库（CLI 项目）。
- **最小 feature**：每个依赖只开启必要 feature（如 `reqwest` 只开 `json, rustls-tls, stream`）。
- **去重**：定期 `cargo tree --duplicates`，统一版本。
- **许可证**：`cargo deny check licenses` 限制为 MIT / Apache-2.0 / BSD / ISC 等。

---

## 13. 备选方案与权衡记录

| 决策点 | 选择 | 备选 | 权衡 |
|--------|------|------|------|
| 运行时 | `tokio` | `async-std` | 生态广度胜出 |
| HTTP | `reqwest`+`rustls` | `hyper` 裸用 | 开发效率与控制力平衡 |
| TUI | `ratatui` | `cursive` | `ratatui` 更现代、无状态刷新模型 |
| 配置 | `toml` | `yaml`/`json` | Rust 生态亲和度 |
| 错误 | `thiserror`+`anyhow` | `snafu` | 主流、低学习成本 |
| Token 计数 | `tiktoken-rs` | 在线 API | 离线、低延迟 |
| Linux 沙箱 | `landlock`+`libseccomp` | `bubblewrap`（bwrap） | `landlock` 纯 Rust、内核原生无需外部二进制；bwrap 需 SUID 安装、跨发行版不可靠 |
| macOS 沙箱 | `sandbox-exec`（Seatbelt） | 自实现 sandbox kit | 系统自带、零依赖；profile 语法受限但够用 |
| Windows 沙箱 | `windows` 受限令牌 + Job Object | AppContainer | Job Object + DACL 更成熟可控；AppContainer 权限模型复杂 |
| MCP 客户端 | `rmcp`（M5+） | 自实现 http/stdio | 官方 SDK 协议跟进快；M4 先用自实现 stdio 薄封装降低风险 |
| 文件锁 | `fs2` | `flock` 裸调 | 跨平台封装、API 稳定 |

---

## 14. 版本与升级策略

- 依赖升级遵循"小步快跑"，每月一次依赖 PR。
- 破坏性升级单独 PR，CI 全量回归。
- MSRV 提升需 RFC 评审，至少保留一个版本周期迁移窗口。
