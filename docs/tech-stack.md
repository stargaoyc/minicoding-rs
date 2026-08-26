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
| MSRV | 1.99+ | edition 2024 稳定门槛（1.99 持续包含 edition 2024 与稳定的 `async fn in trait`，并对最新 std/lib API 提供支持） |
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
| 异步 trait | 原生 `async fn in trait` + 手写 `Pin<Box<dyn Future + Send>>` 返回类型（BoxFuture/BoxStream） | - | `trait-variant` 宏 / `async-trait` | edition 2024 后原生稳定；`dyn` 兼容由手写返回类型保证——更显式、不引入过程宏依赖（决策记录见 §13.1；`trait-variant` 曾评估、未采用） |
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
| LSP server（M8） | `tower-lsp` | 自研 JSON-RPC stdio 薄封装 | 主流 Rust LSP 框架（基于 tower），MIT/Apache-2.0；LSP 协议方法集庞大，自研易出错且落后标准；`tower-lsp` 提供类型安全的 trait 派发与生命周期管理。依赖隔离在 `minicoding-server`（feature gate `lsp`） |
| HTTP server（M9） | `axum` 0.8 | 自研 HTTP 路由 | tower 生态标准 web 框架，MIT/Apache-2.0；SSE/JSON-RPC over HTTP/静态托管（tower-http fs）开箱即用；自研路由+流式响应易出错。依赖隔离在 `minicoding-server`（DOC-7，2026-08-25 R2 审查补条目——design.md 此前引用本表无 axum 行） |

### 4.1 前端与桌面应用（M9，低优先级）

> **范围说明**：M9 为可选里程碑，优先级低于 M5–M8。前端栈选型遵循"最新稳定 + 类型安全 + 编译期优化"原则，技术锁定到当前主流版本，后续随社区演进升级。所有前端代码与 Rust 后端通过 `minicoding-server` 提供的 HTTP/SSE JSON-RPC 接口（见 `design.md` §24）通信，**不嵌入 Rust 进程**，保证后端可独立作为 CLI/SDK 使用。

| 用途 | 选择 | 版本 | 理由 |
|------|------|:---:|------|
| 框架 | React | 19.2 | 主流稳定，Concurrent 渲染 + Suspense 适配流式 LLM 输出；19.x 的 Actions/useTransition 与 React Compiler 配合降低手写 memo 心智负担 |
| 编译器 | React Compiler | latest（RC） | 编译期自动 memoization，减少 `useMemo`/`useCallback` 样板；与 React 19 深度集成 |
| 语言 | TypeScript | 7.0 | 类型安全；7.x 持续改进类型推导与性能 |
| 构建 | Vite (Rolldown) | 8.1 | Rolldown 后端（Rust 实现）替代 Rollup，构建速度显著提升；HMR 体验佳 |
| 路由 | TanStack Router | 1.170（**未采用**） | 单页状态切换无需路由库；引入前需修订本表与 AGENTS §8.2（2026-08-23 审查决策，DOC-8 对齐） |
| 数据获取 | TanStack Query | 5.101 | 服务端状态管理（缓存/重试/失效），与 JSON-RPC 后端契合，流式 SSE 用 `useQuery` + 增量更新 |
| 客户端状态 | Zustand | 5.0 | 轻量级全局状态（UI 主题、面板开关等），避免 Redux 样板；与 TanStack Query 职责正交 |
| Schema 校验 | Zod | 4.4 | 运行时类型校验，与 TypeScript 7 类型双向推导；用于 JSON-RPC 请求/响应 schema 校验，对接 `minicoding-protocol` 的 DTO |
| 组件库 | shadcn/ui | latest | 基于 Radix UI 的可定制组件（复制粘贴源码而非 npm 依赖），完全可控，与 Tailwind v4 深度集成 |
| 样式 | Tailwind CSS | v4 | 原子化 CSS，v4 改用 Oxide 引擎（Rust 实现）显著提升构建速度；CSS-first 配置 |
| 动画 | Framer Motion | latest | 声明式动画，与 React 19 兼容；用于面板过渡、权限弹窗动效 |
| Lint | oxlint | latest | Rust 实现的 ESLint 替代，速度数十倍提升；规则集与 ESLint 兼容 |
| 格式化 | oxfmt | latest | Rust 实现的 Prettier 替代，速度显著提升；与 oxlint 同源（Oxc 项目） |
| 桌面壳 | Tauri | 2.x | Rust 实现的桌面应用壳，体积远小于 Electron（5–10MB vs 100MB+）；2.x 支持 mobile，IPC 用 Rust 命令直接调用，性能优 |

> **说明**：oxlint/oxfmt/Vite (Rolldown)/Tailwind v4 均为 Rust 实现的工具链，与本项目 Rust 后端形成"全 Rust 工具链"一致性，构建/Lint/格式化速度均显著优于传统 Node 工具链。

#### 4.1.1 前端与后端的通信

- **协议**：HTTP/SSE JSON-RPC 2.0（复用 `minicoding-protocol` 的 wire types，见 `modules.md` §15）；
- **传输**：
  - Web 模式：浏览器 → HTTPS → `minicoding-server`（`minicoding serve --http`）；
  - 桌面模式（Tauri）：前端 → Tauri IPC → Rust sidecar 进程（即 `minicoding-server`），sidecar 与前端同进程组，避免跨进程序列化开销大的操作走 IPC、其他走 HTTP；
- **流式**：SSE 推送 `Event::Token`/`Event::ToolCall`/`Event::PermissionRequest`，前端用 TanStack Query 的 `useQuery` + `queryClient.setQueryData` 增量更新；
- **权限交互**：`PermissionPrompt` 经 SSE 推到前端，弹出 shadcn/ui Dialog，用户决策经 JSON-RPC `permission.resolve` 回传；
- **离线/本地**：桌面模式默认连接本地 sidecar；Web 模式可连远程 `minicoding-server`（需 CORS 配置）。

#### 4.1.2 为何选 Tauri 而非 Electron

| 维度 | Tauri 2.x | Electron |
|------|-----------|----------|
| 体积 | 5–10MB | 100MB+ |
| 内存 | 系统 webview | 内置 Chromium + Node |
| IPC | Rust 命令直接调用 | Node IPC |
| 安全 | 默认禁用远程内容，CSP 严格 | 默认允许 Node 集成，需手动收紧 |
| 与本项目契合 | Rust 后端可直接作为 sidecar/embedded | 需启动额外 Node 进程 |
| 移动端 | 2.x 支持 iOS/Android | 不支持 |

Tauri 与本项目"Rust 一等公民"理念一致，且体积/内存/安全均优于 Electron。

---

## 5. 文件与代码处理

| 用途 | crate | 理由 |
|------|-------|------|
| 文件系统 | `std::fs` + `tokio::fs` | 够用即可 |
| 路径处理 | `std::path` + `camino` | `camino` 提供 `Utf8PathBuf`，避免 OS 字符集边界问题 |
| Glob 匹配 | `globset` | `ripgrep` 同源，性能强 |
| 正则 | `regex` | 标配；不支持回溯但够用 |
| 目录遍历 | `ignore` | 尊重 `.gitignore`，与 `ripgrep` 行为一致 |
| 文件监听 | `notify` 8 | S-22 配置热更新 `ConfigWatcher` 监听 `~/.minicoding/config.toml` 变更（500ms debounce + best-effort 降级），广播 `Event::ConfigChanged`；由 `minicoding-core` 引入 |
| tar 打包 | `tar` 0.4 | S-05 备份功能打包 `~/.minicoding/` 为 tar 归档；由 `minicoding-cli` 引入 |
| gzip 压缩 | `flate2` 1 | S-05 备份 gzip 压缩 tar 归档为 `.tar.gz`；由 `minicoding-cli` 引入 |
| Diff 生成 | `similar` | Myers diff 算法主流实现，`edit`/`multiedit` 工具展示变更、`/undo` 预览用 |
| HTML→Markdown | `htmd` | `web.fetch` 工具把抓取的 HTML 转 Markdown，纯 Rust 无 C 依赖 |
| 有序 ID | `ulid` | `task_id`/`op_id` 用 ULID（字典序可排序，比 UUID 更适合按时间顺序列出） |
| 哈希比对 | `sha2` | `Journal` 冲突检测比对文件内容 hash（C-28） |

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
| 配置加载 | 自实现（`toml` 直接解析） | 单一 user 级 `config.toml` + env/CLI 叠加；project 分层为规划项（见 `roadmap.md`） |
| 环境变量 | `std::env` | `env:VAR` / `env:VAR:-fallback` 语法内置解析；`figment` 曾评估、未采用（单一文件场景收益不足，避免重依赖） |
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
| 属性测试 | `proptest` | `Message` JSON roundtrip、path sandbox 不变量（features Q-05） |
| 性能基准 | `criterion` | 压缩管道 100/500/1000 消息基准（features Q-06） |
| 模糊测试（可选） | `cargo-fuzz` | 解析器（SSE/JSON）模糊测试 |

---

## 11. 安全相关（沙箱为一等公民，见 `security.md` §8）

OS 级沙箱升级为一等公民后，安全相关依赖按"应用层 + 内核级"两道防线组织。沙箱驱动集中在 `minicoding-sandbox` crate，平台 C 绑定不污染 core。

| 用途 | crate / 方案 | 平台 | 理由 |
|------|--------------|:---:|------|
| TLS | `rustls` | 全平台 | 避免系统 OpenSSL，便于静态编译 |
| 凭证存储 | OS keychain（`keyring`） / 文件 0600 | 全平台 | 不把密钥写进配置明文 |
| 应用层路径沙箱 | `std::path::canonicalize` + `camino` | 全平台 | 防目录穿越（第一道防线，`security.md` §3） |
| 跨平台沙箱统一 API | ~~`sandbox-run`~~（**已弃用**） | Linux+macOS | systemd 风格 API（`ProtectSystem`/`ReadWritePaths`/`PrivateNetwork`），原生支持 `apply_sandbox` 在子进程 fork 后 exec 前调用，与 `tokio::process` 兼容；内部封装 Landlock ruleset 与 macOS sandbox profile 生成。**弃用原因**：EUPL-1.2 许可证不合规（AGENTS.md §2.7），已由自研轻量驱动替代（Linux landlock `pre_exec` / macOS `sandbox_init` / Windows Job Object，见 `minicoding-sandbox/src/lib.rs` 顶部注释） |
| Linux 文件系统沙箱 | `landlock` | Linux 5.13+ | 官方 rust-landlock，内核 LSM 限制可写范围；纯 Rust 绑定无 C 依赖，由自研 pre_exec 胶水直连 |
| Linux 系统调用过滤 | `libseccomp`（已接，opt-in feature `seccomp` 默认关） | Linux | seccomp-bpf deny-list 系统调用（禁 `ptrace`/`mount`/`reboot`/`kexec_load` 等）；需系统 C 库 libseccomp-dev（见 §13 决策记录、security.md §8.11） |
| macOS 沙箱 | `sandbox_init`(3) FFI（自研胶水） | macOS 12+ | Seatbelt 框架：父进程生成 profile 临时文件，子进程 fork 后 exec 前经 FFI 加载；原 ~~`sandbox-run`~~ 方案已随之弃用，无需手写 profile 解析 |
| Windows 受限令牌 | `windows` crate | Windows 10+ | 受限 token + Job Object + DACL 限制写路径；成熟度低于 macOS/Linux，初期可降级为应用层 + 用户提示 |
| 进程硬化 | `libc`（`PR_SET_DUMPABLE`/`RLIMIT_CORE`） | Linux/Unix | pre-main 禁 ptrace/core dump，清 `LD_*`/`DYLD_*` |
| 跨进程文件锁 | `fs2` | 全平台 | 会话文件互斥（`data-model.md` §10） |
| 文件权限收紧 | `std::fs` + `cfg!(unix)` `chmod 0600/0700` | Unix | `~/.minicoding/` 与会话文件权限收紧 |

> **平台检测策略**：`minicoding-sandbox::detect_driver()` 编译期按 `cfg!(target_os)` 选实现，运行期 `landlock_available()` 探测内核支持（Linux）。无可用硬隔离时（如 Windows 早期版本、不支持 Landlock 的旧内核）返回 `NoopDriver`（来自 core）并打 `warn`，依赖容器自身隔离（对应 `ExternalSandbox` 策略）。`landlock` 通过 cargo `[target.'cfg(target_os = "linux")'.dependencies]` 条件引入，非 Linux 平台不编译；macOS 经 FFI 直连 libsystem 无外部 crate。统一调用入口由自研 `detect_driver()` 工厂 + `SandboxDriver::apply(cmd)`（pre_exec 胶水）承担。

> **平台优先级（Linux 先行）**：沙箱与核心 Runtime 的多平台支持分阶段交付：
> - **M0-M4（Linux 先行）**：沙箱仅实现 Linux（`landlock` 直连 + 自研 pre_exec 胶水），CI matrix 只跑 Linux。macOS/Windows 在此阶段编译可用但沙箱降级为 `NoopDriver` + 应用层权限 + 用户提示（不阻塞 MVP）。
> - **M5+（macOS 补齐）**：补齐 macOS `sandbox_init`(3) FFI（Seatbelt）实现与 CI matrix。
> - **M6+（Windows 补齐）**：补齐 Windows 受限令牌 + Job Object 实现。
> - **理由**：Linux 是 AI Coding 的主战场（CI/容器/服务器），Landlock 最成熟；macOS/Windows 沙箱成熟度低且非核心场景，推迟到最后避免阻塞 MVP。`detect_driver()` 工厂已按平台抽象统一入口，后续补齐只是平台实现填充，不涉及架构变更。

### 11.1 Hooks 与 MCP 相关

| 用途 | crate / 方案 | 理由 |
|------|--------------|------|
| Hooks 脚本执行 | `tokio::process::Command` | Hooks 以"外部可执行 + JSON over stdio"为主协议（见 `hooks.md` §3），无需额外 crate |
| Hook JSON 协议 | `serde_json` | stdin/stdout 单行 JSON，复用既有依赖 |
| MCP 客户端 | `rmcp` 2.2 | 官方 Rust MCP SDK（modelcontextprotocol/rust-sdk），对齐 MCP 2025-11-25 spec；直接用 `transport-child-process`（stdio）+ `transport-streamable-http-client-reqwest`（HTTP+rustls）+ OAuth；M4 一步到位，**不再"自实现 stdio 薄封装"过渡** |
| MCP server 暴露 | `rmcp` 2.2 | `minicoding serve --as-mcp-server` 把内置工具暴露给其他 Agent（用 `#[tool]` 宏 + `transport-io`，阶段 8） |
| 工具检索（阶段 6+） | 自实现 BM25 | MCP 工具多时按需检索（见 `design.md` §19.6），不引入向量依赖 |

> **依赖隔离**：`rmcp` 含完整网络栈与 OAuth 流程，仅在 `minicoding-mcp` crate 引入；`minicoding-core` 仅定义 `McpClient` trait，不依赖 `rmcp`，保持 core 轻量。Hooks 无新增依赖，复用 `tokio::process`。

---

## 12. 依赖治理策略

- **审计**：`cargo audit` + `cargo deny` 接入 CI。
- **版本锁定**：`Cargo.lock` 提交到仓库（CLI 项目）。
- **最小 feature**：每个依赖只开启必要 feature（如 `reqwest` 只开 `json, rustls-tls, stream`）。
- **去重**：定期 `cargo tree --duplicates`，统一版本。
- **许可证**：`cargo deny check licenses` 限制为 MIT / Apache-2.0 / BSD / ISC 等。
- **供应链硬化**（参考 Pi 项目）：
  - `cargo supply-chain` 检查依赖来源可信度；
  - 依赖更新 PR 需人工审查，CI 自动检测版本跳跃；
  - 避免引入"同日发布"依赖（`min-release-age` 理念），降低供应链投毒风险；
  - 重依赖（`reqwest`/`rmcp`/`landlock`）升级需附加 changelog 审查。
- **凭证环境变量语法**（参考 AstrCode）：
  - 统一使用 `env:VAR_NAME` 语法引用环境变量（如 `api_key = "env:OPENAI_API_KEY"`）；
  - 支持 `env:VAR_NAME:-fallback` 回退语法；
  - 保留 `${VAR_NAME}` 兼容（MCP env 段）；
  - 环境变量缺失时启动解析失败并给出明确错误。

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
| 跨平台沙箱统一 API | ~~`sandbox-run`~~（初选，因 EUPL-1.2 弃用）→ 自研轻量 pre_exec 胶水 | 自研 seatbelt profile + landlock ruleset 胶水 | 初评 `sandbox-run` 封装跨平台细节（Landlock ruleset 构建、macOS profile 生成）易用；后因 EUPL-1.2 许可证不合规弃用，改为自研薄胶水（仅封装子进程启动路径，ruleset 构建仍复用官方 crate），维护面可控 |
| Linux 沙箱底层 | `landlock` 直连（`libseccomp` 已接[opt-in]） | `bubblewrap`（bwrap） | `landlock` 纯 Rust、内核原生无需外部二进制；bwrap 需 SUID 安装、跨发行版不可靠 |
| macOS 沙箱 | `sandbox_init`(3) FFI（自研胶水） | 自实现 sandbox kit / 裸 `sandbox-exec` | Seatbelt 内置框架经 FFI 加载 profile，无外部依赖；裸 `sandbox-exec` 需手写 profile 字符串易错 |
| Windows 沙箱 | `windows` 受限令牌 + Job Object | AppContainer | Job Object + DACL 更成熟可控；AppContainer 权限模型复杂 |
| MCP 客户端 | `rmcp` 2.2（官方，M4 一步到位） | 自实现 http/stdio | 官方 SDK 协议跟进快、对齐 2025-11-25 spec、含 `#[tool]` 宏与 schemars；自实现易落后、维护成本高 |
| LSP server | `tower-lsp`（M8） | 自研 JSON-RPC stdio 薄封装 | LSP 协议方法集庞大（`textDocument/*`/`workspace/*`/`window/*`），`tower-lsp` 提供类型安全派发与生命周期管理；自研易出错且落后 LSP spec；与 ACP 共享 `minicoding-protocol` wire types，仅语义映射层不同 |
| 文件锁 | `fs2` | `flock` 裸调 | 跨平台封装、API 稳定 |
| 配置热更新策略（M-12，R-04） | **白名单 + turn 边界生效**：`ConfigWatcher` 仅探测变更并广播 `Event::ConfigChanged`；`Runtime` 在每次 `run_turn` 开头读 config.toml，**仅当文件中显式存在**白名单 key（`provider.model`/`context.turn_timeout_sec`/`tools.parallel_reads`）时才覆盖运行期配置；非白名单字段变更仅 warn 提示重启 | 全量热重载 / 完全不做热更新 | **不做全量热重载**：C-29 压缩熔断状态机与 provider 重建依赖构造时配置，热换不安全；白名单 presence 判断避免 serde default（文件缺字段补默认值）覆盖 CLI/env 覆盖值。`provider.model` 热生效依赖 `build_request_body` 改用 `req.params.model`（tokenizer 仍为构造时快照，接受此限制）。`Runtime.config` 用 `std::sync::RwLock` 锁保护（`build_dispatch_config` 是同步 fn 无法 `read().await`），读取点均为短临界区、guard 不跨 await |

### 13.1 异步 trait 的 dyn 兼容：手写 `Pin<Box<dyn Future + Send>>`（决策记录）

**现状（与代码一致）**：全项目含 `async fn` 的 trait（LlmProvider/Tool/PermissionPolicy/PermissionPrompter/
ContextManager/Hook/HookRegistry/SandboxDriver/ProjectDocLoader/Journal/McpClient/Storage）均以
手写 `Pin<Box<dyn Future + Send>>` / `BoxStream` 返回类型定义（范式见 `minicoding-core`
`provider/trait.rs` 头注），Runtime 据此持有 `Arc<dyn Trait>` 做运行时装配。

**为何必须特殊处理**：Rust 2024 稳定了 `async fn in trait`，但**未稳定 `dyn` 兼容的 async trait**。
Runtime 需要动态派发（feature gate 按需装配实现 crate），原生 `async fn in trait` 不是 object-safe，
必须在 trait 定义处显式擦除为 boxed future。

**方案对比**：

| 方案 | 结论 | 理由 |
|------|------|------|
| 手写 `Pin<Box<dyn Future + Send>>` 返回类型 | **采用** | 显式可见、零宏依赖、签名即真相；噪声通过"每个 trait 集中在一个文件 + 头注说明范式"控制 |
| `trait-variant` 宏 | 曾评估、未采用 | 签名处宏生成双 trait，实际形态不可见；为一个纯编译期变换引入过程宏依赖不划算（2026-08-23 审查 §3 清理死依赖时确认实现从未使用该宏） |
| `async-trait` | 否决 | box future 运行时堆分配 + 属性宏侵入；已属废弃路径 |
| 不用 `dyn` 全用泛型 | 否决 | 编译时间长、二进制体积大、Runtime 无法运行时装配实现 crate |

**迁移路径**：当 Rust 原生支持 object-safe async trait（RFC 3668 `dyn*` 或后续）稳定后：
1. trait 直接定义 `async fn`，移除手写返回类型；
2. Runtime 持有 `Arc<dyn Trait>` 不变；
3. 验证：`cargo test` 全量回归 + `cargo bench` 确认无性能回退。

`AGENTS.md` §2.1 已同步此约定。

### 13.2 消息追加 fsync 策略：保持逐条 fsync，暂不做 turn 级批量（M-13 评估）

**现状**：`JsonlStorage::append` 每条消息写入后调用 `sync_all()`（fsync），保证"消息先写盘再入上下文"的崩溃安全不变量（见 `design.md` §2.2）——进程/系统崩溃后磁盘状态是已确认消息的前缀，无半行、无丢失。

**备选方案**：turn 级批量 fsync（turn 结束或每 N 条消息统一刷盘）。

| 维度 | 逐条 fsync（现状） | turn 级批量 |
|------|------------------|------------|
| 崩溃安全 | 单条消息粒度（最多丢最后 1 条未刷盘消息） | 最坏丢整个 turn 的消息（LLM 已产出但未落盘） |
| IO 开销 | 每 append 一次 fsync（CLI 场景每秒个位数 append，实测非瓶颈） | 降低约 N 倍 fsync 调用 |
| 实现复杂度 | 无 | 需 dirty 追踪 + turn 边界 flush 钩子 + 与 M-01 会话锁协同；跨进程崩溃语义变复杂 |

**决策**：**保持逐条 fsync**。理由：(1) CLI/TUI 场景 append 频率低（人机交互节奏），fsync 不是可观测瓶颈，无 profiling 证据前不做优化（避免过早优化）；(2) 批量方案把崩溃窗口从"1 条"扩大到"1 个 turn"，与 C-28（Journal 回滚依赖 after 快照完整）和 resume 一致性语义冲突，收益/风险比不划算；(3) 未来若出现高频自动写场景（如 subagent 并发落盘、评测 harness 批量回放），以 `Storage` 后端替换方式实现批量策略（契约测试保证行为等价），不改上层。触发条件：profiling 显示 fsync 占 turn 延迟 >10% 时重评。

---

## 14. 版本与升级策略

- 依赖升级遵循"小步快跑"，每月一次依赖 PR。
- 破坏性升级单独 PR，CI 全量回归。
- MSRV 提升需 RFC 评审，至少保留一个版本周期迁移窗口。
