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
| LSP server（M8） | `tower-lsp` | 自研 JSON-RPC stdio 薄封装 | 主流 Rust LSP 框架（基于 tower），MIT/Apache-2.0；LSP 协议方法集庞大，自研易出错且落后标准；`tower-lsp` 提供类型安全的 trait 派发与生命周期管理。依赖隔离在 `minicoding-server`（feature gate `lsp`） |

### 4.1 前端与桌面应用（M9，低优先级）

> **范围说明**：M9 为可选里程碑，优先级低于 M5–M8。前端栈选型遵循"最新稳定 + 类型安全 + 编译期优化"原则，技术锁定到当前主流版本，后续随社区演进升级。所有前端代码与 Rust 后端通过 `minicoding-server` 提供的 HTTP/SSE JSON-RPC 接口（见 `design.md` §16）通信，**不嵌入 Rust 进程**，保证后端可独立作为 CLI/SDK 使用。

| 用途 | 选择 | 版本 | 理由 |
|------|------|:---:|------|
| 框架 | React | 19.2 | 主流稳定，Concurrent 渲染 + Suspense 适配流式 LLM 输出；19.x 的 Actions/useTransition 与 React Compiler 配合降低手写 memo 心智负担 |
| 编译器 | React Compiler | latest（RC） | 编译期自动 memoization，减少 `useMemo`/`useCallback` 样板；与 React 19 深度集成 |
| 语言 | TypeScript | 7.0 | 类型安全；7.x 持续改进类型推导与性能 |
| 构建 | Vite (Rolldown) | 8.1 | Rolldown 后端（Rust 实现）替代 Rollup，构建速度显著提升；HMR 体验佳 |
| 路由 | TanStack Router | 1.170 | 类型安全路由（路由参数完全类型推导），无路由配置文件，比 React Router 更适合 SPA |
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
| 跨平台沙箱统一 API | `sandbox-run` | Linux+macOS | systemd 风格 API（`ProtectSystem`/`ReadWritePaths`/`PrivateNetwork`），原生支持 `apply_sandbox` 在子进程 fork 后 exec 前调用，与 `tokio::process` 兼容；内部封装 Landlock ruleset 与 macOS sandbox profile 生成，**不自研胶水** |
| Linux 文件系统沙箱 | `landlock` | Linux 5.13+ | 官方 rust-landlock，内核 LSM 限制可写范围；纯 Rust 绑定无 C 依赖，由 `sandbox-run` 底层调用 |
| Linux 系统调用过滤 | `libseccomp` | Linux | seccomp-bpf 白名单系统调用（禁 `ptrace`/`mount`/`reboot`/`kexec_load`），与 `sandbox-run` 叠加 |
| macOS 沙箱 | `sandbox-run`（封装原生 sandbox 框架） | macOS 12+ | Seatbelt 框架，由 `sandbox-run` 生成 profile 并应用，无需手写 profile 字符串 |
| Windows 受限令牌 | `windows` crate | Windows 10+ | 受限 token + Job Object + DACL 限制写路径；成熟度低于 macOS/Linux，初期可降级为应用层 + 用户提示 |
| 进程硬化 | `libc`（`PR_SET_DUMPABLE`/`RLIMIT_CORE`） | Linux/Unix | pre-main 禁 ptrace/core dump，清 `LD_*`/`DYLD_*` |
| 跨进程文件锁 | `fs2` | 全平台 | 会话文件互斥（`data-model.md` §10） |
| 文件权限收紧 | `std::fs` + `cfg!(unix)` `chmod 0600/0700` | Unix | `~/.minicoding/` 与会话文件权限收紧 |

> **平台检测策略**：`minicoding-sandbox::detect_driver()` 编译期按 `cfg!(target_os)` 选实现，运行期 `sandbox_run::landlock_available()` 探测内核支持。无可用硬隔离时（如 Windows 早期版本、不支持 Landlock 的旧内核）返回 `NoopDriver`（来自 core）并打 `warn`，依赖容器自身隔离（对应 `ExternalSandbox` 策略）。`landlock` 与 `libseccomp` 通过 cargo `[target.'cfg(target_os = "linux")'.dependencies]` 条件引入，非 Linux 平台不编译。`sandbox-run` 本身跨 Linux+macOS，统一了 `apply_sandbox` 调用入口。

> **平台优先级（Linux 先行）**：沙箱与核心 Runtime 的多平台支持分阶段交付：
> - **M0-M4（Linux 先行）**：沙箱仅实现 Linux（`sandbox-run` + `landlock` + `libseccomp`），CI matrix 只跑 Linux。macOS/Windows 在此阶段编译可用但沙箱降级为 `NoopDriver` + 应用层权限 + 用户提示（不阻塞 MVP）。
> - **M5+（macOS 补齐）**：补齐 macOS `sandbox-run`（Seatbelt）实现与 CI matrix。
> - **M6+（Windows 补齐）**：补齐 Windows 受限令牌 + Job Object 实现。
> - **理由**：Linux 是 AI Coding 的主战场（CI/容器/服务器），Landlock 最成熟；macOS/Windows 沙箱成熟度低且非核心场景，推迟到最后避免阻塞 MVP。`sandbox-run` 跨平台 API 已统一，后续补齐只是平台实现填充，不涉及架构变更。

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
| 跨平台沙箱统一 API | `sandbox-run` | 自研 seatbelt profile + landlock ruleset 胶水 | `sandbox-run` 封装跨平台细节（Landlock ruleset 构建、macOS profile 生成），systemd 风格 API 易用；自研胶水维护成本高、易出错 |
| Linux 沙箱底层 | `landlock`+`libseccomp`（由 sandbox-run 调用） | `bubblewrap`（bwrap） | `landlock` 纯 Rust、内核原生无需外部二进制；bwrap 需 SUID 安装、跨发行版不可靠 |
| macOS 沙箱 | `sandbox-run`（封装 Seatbelt） | 自实现 sandbox kit / 裸 `sandbox-exec` | `sandbox-run` 统一 API 跨平台；裸 `sandbox-exec` 需手写 profile 字符串易错 |
| Windows 沙箱 | `windows` 受限令牌 + Job Object | AppContainer | Job Object + DACL 更成熟可控；AppContainer 权限模型复杂 |
| MCP 客户端 | `rmcp` 2.2（官方，M4 一步到位） | 自实现 http/stdio | 官方 SDK 协议跟进快、对齐 2025-11-25 spec、含 `#[tool]` 宏与 schemars；自实现易落后、维护成本高 |
| LSP server | `tower-lsp`（M8） | 自研 JSON-RPC stdio 薄封装 | LSP 协议方法集庞大（`textDocument/*`/`workspace/*`/`window/*`），`tower-lsp` 提供类型安全派发与生命周期管理；自研易出错且落后 LSP spec；与 ACP 共享 `minicoding-protocol` wire types，仅语义映射层不同 |
| 文件锁 | `fs2` | `flock` 裸调 | 跨平台封装、API 稳定 |

### 13.1 `trait-variant` 宏的风险管理

`#[trait_variant::make(Trait: Send)]` 在全项目 trait（LlmProvider/Tool/PermissionPolicy/PermissionPrompter/ContextManager/Hook/HookRegistry/SandboxDriver/ProjectDocLoader/Journal/McpClient/Storage）上使用，存在以下风险，需显式管理：

**为何必须用**：Rust 2024 稳定了 `async fn in trait`，但**未稳定 `dyn` 兼容的 async trait**。Runtime 需持有 `Arc<dyn Trait>` 做动态派发（运行时装配实现 crate），原生 `async fn in trait` 的 trait 不是 object-safe。`trait-variant` 生成 Send 变体使 trait 可作 `dyn` 对象，是目前唯一不引入运行时开销（对比 `async-trait` 的 box future）的方案。

**风险清单**：

| 风险 | 说明 | 缓解 |
|------|------|------|
| 第三方宏依赖 | `trait-variant` crate 版本锁定 | 锁定在 `1.x`，CI `cargo audit` 监控；宏本身轻量（纯过程宏，无运行时依赖） |
| 双 trait 生成 | 每个 trait 编译期生成原始 + Send 变体 | 编译开销可接受；`cargo check` 时间未显著劣化 |
| 迁移成本 | Rust 原生支持后需移除宏 | 迁移点集中：trait 定义处 + Runtime 持有 `Arc<dyn>` 处；影响面可控 |
| 语法侵入 | `#[trait_variant::make]` 注解散布全项目 | 集中在 `minicoding-core` 的 trait 定义模块（11 个文件），不扩散到实现 crate |

**迁移路径**：当 Rust 原生支持 `dyn*` 或 object-safe async trait（RFC 3668 `dyn*` 或后续）稳定后：
1. 移除 `#[trait_variant::make]` 注解；
2. trait 直接定义 `async fn`，保持 object-safe；
3. Runtime 持有 `Arc<dyn Trait>` 不变；
4. 验证：`cargo test` 全量回归 + `cargo bench` 确认无性能回退。

**替代方案对比**（已否决）：

| 方案 | 否决理由 |
|------|---------|
| `async-trait` | box future 运行时堆分配，热路径（Agent 循环每轮调用）性能差；已属废弃路径 |
| 手写 `Pin<Box<dyn Future>>` 返回类型 | 侵入性强、噪声大、易错 |
| 不用 `dyn` 全用泛型 | 编译时间长、二进制体积大、Runtime 无法运行时装配实现 crate（feature gate 失效） |

**决策**：M0-M5 使用 `trait-variant`；在 `roadmap.md` M6 评审节点检查 Rust 官方进展，若原生方案稳定则纳入 M7 迁移 task。`AGENTS.md` §2.1 已约束不引入 `async-trait`。

---

## 14. 版本与升级策略

- 依赖升级遵循"小步快跑"，每月一次依赖 PR。
- 破坏性升级单独 PR，CI 全量回归。
- MSRV 提升需 RFC 评审，至少保留一个版本周期迁移窗口。
