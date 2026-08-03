<!--
文档性质：minicoding-rs 项目构建指南
适用读者：新贡献者、CI 维护者、发布管理员
关联文档：
  - docs/tech-stack.md（技术选型、系统依赖、平台支持）
  - docs/getting-started.md（快速上手）
  - docs/modules.md（crate 结构、特性门控）
  - docs/m9-design.md（Web/桌面构建工具链）
  - AGENTS.md（编码规范、依赖治理）
维护规则：改代码必改文档（见 AGENTS.md §4.1）；引用用相对路径或 §章节号。
-->

# minicoding-rs 构建指南

本文档描述 `minicoding-rs` 项目从源码构建到发布产物的完整流程，覆盖 Rust 后端 workspace、Web 前端、Tauri 桌面应用、跨平台编译、CI/CD 流水线与常见问题排查。所有命令均经过 CI 验证（见 `.github/workflows/ci.yml`）。

> **阅读约定**：本文引用的 `tech-stack.md`、`modules.md`、`getting-started.md`、`m9-design.md`、`security.md` 等均为 `docs/` 目录下的相对路径。`AGENTS.md` 为项目根目录文件。

---

## 1. 前言

### 1.1 文档目的

`minicoding-rs` 是一个 Rust 实现的终端 AI Coding 助手，包含 18 个 Cargo workspace crate（M0–M9 范围）与 1 个独立 npm 前端项目（`crates/minicoding-web/`），支持 CLI、TUI、Web、桌面四种部署形态。本文档旨在为开发者提供：

- 从零搭建构建环境的完整步骤；
- 各 crate 的 feature gate 语义与按需编译方法；
- 跨平台（Linux/macOS/Windows）构建注意事项；
- 发布构建（cargo-dist + GitHub Releases）流程；
- 构建优化（sccache、mold、profile 调优）建议；
- CI/CD 流水线门禁说明；
- 常见构建错误的排查指南。

### 1.2 适用读者

| 读者 | 推荐章节 |
|------|---------|
| 新贡献者（首次构建） | §1–§4 |
| 领域 crate 开发者 | §4–§5 |
| 前端/桌面开发者 | §6 |
| 发布管理员 | §8、§12 |
| CI 维护者 | §11–§12 |
| 遇到构建问题的开发者 | §13 |

### 1.3 项目形态概览

| 形态 | crate / 项目 | 构建工具 | 产物 |
|------|-------------|---------|------|
| CLI | `minicoding-cli` | cargo | `minicoding` 二进制 |
| TUI | `minicoding-tui` | cargo | `minicoding-tui` 二进制 |
| HTTP/SSE Server | `minicoding-server` | cargo | `minicoding-server` 二进制 |
| 嵌入 SDK | `minicoding-sdk` | cargo | library crate |
| Web 前端 | `minicoding-web/` | npm + Vite | `dist/` 静态资源 |
| 桌面应用 | `minicoding-desktop` | cargo + Tauri | `.dmg`/`.msi`/`.AppImage` |

crate 结构与依赖方向见 `docs/modules.md` §0.1–§0.2。

---

## 2. 环境准备

### 2.1 Rust 工具链

#### 2.1.1 版本要求

| 项 | 要求 | 来源 |
|----|------|------|
| edition | 2024 | `Cargo.toml` `[workspace.package]`、`AGENTS.md` §2.1 |
| MSRV | 1.99+ | `Cargo.toml` `rust-version = "1.99"` |
| 工具链通道 | nightly（当前） | `rust-toolchain.toml` |
| 组件 | `rustfmt`、`clippy`、`llvm-tools-preview` | `rust-toolchain.toml` |

> **为何用 nightly**：项目使用 `let chains`（`if .. && let Some(..) = ..`）等 Rust 2024 edition 特性，要求 rustc ≥ 1.99。在 `rust-toolchain.toml` 编写时 1.99 仍在 nightly/beta 通道，故固定 nightly。stable 1.99 发布后可切换为 `channel = "stable"`（见 `rust-toolchain.toml` 注释）。

#### 2.1.2 安装 Rust

```bash
# Unix（Linux/macOS）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Windows PowerShell
irm https://sh.rustup.rs | iex
```

项目根目录的 `rust-toolchain.toml` 会自动触发 rustup 安装指定通道与组件，无需手动 `rustup component add`。

#### 2.1.3 验证工具链

```bash
cd minicoding-rs
rustc --version    # 应为 nightly，版本 >= 1.99
cargo --version
rustfmt --version
cargo clippy --version
```

`rust-toolchain.toml` 内容参考：

```toml
# rust-toolchain.toml
[toolchain]
channel = "nightly"
components = ["rustfmt", "clippy", "llvm-tools-preview"]
profile = "minimal"
```

`profile = "minimal"` 仅安装 rustc/cargo，所需组件通过 `components` 显式声明，避免下载未用的 rls/rust-docs。

### 2.2 系统依赖

`minicoding-rs` 的依赖树刻意避免 OpenSSL / protobuf / cmake / clang 等重型 C 依赖（见 `docs/tech-stack.md` §3、§11）：

- HTTP 走 `reqwest` + `rustls`（不依赖系统 OpenSSL）；
- `landlock` crate 是纯 Rust 绑定，无 C 依赖；
- `rmcp` 2.2 是纯 Rust；
- `sandbox-run` 跨平台 Rust 实现。

**唯一需要系统包的是 Linux 下的 `libseccomp`**（用于系统调用过滤）与 Linux 下 Tauri 桌面构建的 webkit2gtk 系列。

#### 2.2.1 系统依赖矩阵

| 平台 | 依赖 | 用途 | 安装命令 |
|------|------|------|---------|
| Linux（启用沙箱） | `libseccomp` 开发头文件 | 系统调用过滤（`minicoding-sandbox`） | 见 §2.2.2 |
| Linux（桌面构建） | `libwebkit2gtk-4.1-dev` 等 | Tauri 2.x WebView 运行时 | 见 §6.3.2 |
| macOS | 无额外系统包 | `sandbox-run` 封装原生 Seatbelt 框架 | — |
| Windows | 无额外系统包 | `windows-sys` crate 使用系统 API | — |
| 全平台 | Git | `git.diff`/`git.apply` 工具 + VCS 目录检测 | 各平台包管理器 |

不需要 protoc（项目用 `serde_json`，不用 protobuf）、不需要 cmake、不需要 clang。

#### 2.2.2 Linux libseccomp 安装

```bash
# Debian / Ubuntu
sudo apt install libseccomp-dev

# Fedora / RHEL
sudo dnf install libseccomp-devel

# Arch Linux
sudo pacman -S libseccomp

# Alpine（容器场景）
apk add libseccomp-dev
```

#### 2.2.3 Tauri 桌面构建依赖（仅 Linux）

Linux 下构建 `minicoding-desktop`（feature `desktop`）需 webview 与 GUI 库：

```bash
sudo apt-get update
sudo apt-get install -y \
    libwebkit2gtk-4.1-dev \
    libgtk-3-dev \
    libglib2.0-dev \
    libsoup-3.0-dev \
    libjavascriptcoregtk-4.1-dev
```

macOS 与 Windows 使用系统内置 WebView（WKWebView / WebView2），无需额外安装。

### 2.3 Git 版本要求

- 最低版本：Git 2.20+（支持 `git diff --output` 等子命令）；
- `minicoding` 的 `git.diff`/`git.apply` 工具通过 `tokio::process::Command` 调用系统 `git`，需在 `PATH` 中可用；
- CI 使用 `actions/checkout@v4`（内置 Git 2.40+），无额外要求。

---

## 3. 获取源码

```bash
# 克隆仓库
git clone https://github.com/minicoding/minicoding-rs.git minicoding-rs
cd minicoding-rs

# 查看当前版本
cat Cargo.toml | grep "^version"   # workspace.package.version = "0.1.0"
```

`Cargo.lock` 已提交到仓库（CLI 项目约定，见 `AGENTS.md` §2.7），首次构建无需手动锁定版本，保证本地与 CI 依赖一致。

仓库根目录结构：

```
minicoding-rs/
├── crates/                    # 18 个 Cargo crate + 1 个 npm 前端项目
│   ├── minicoding-core/       # 抽象层 + Runtime 编排
│   ├── minicoding-cli/        # CLI 前端
│   ├── minicoding-server/     # HTTP/SSE server
│   ├── minicoding-desktop/    # Tauri 桌面壳（M9）
│   ├── minicoding-web/        # Web 前端（独立 package.json）
│   └── ...                    # 其余 14 个领域 crate
├── docs/                      # 设计文档
├── .github/workflows/         # CI/CD 配置
├── Cargo.toml                 # workspace 根配置
├── Cargo.lock                 # 依赖锁定
├── rust-toolchain.toml        # 工具链固定
├── deny.toml                  # cargo-deny 配置
├── cargo-dist.toml            # 发布构建配置
└── AGENTS.md                  # AI 编码约束
```

---

## 4. 基础构建

### 4.1 全量 workspace 构建

```bash
# 构建 workspace 全部 18 个 crate（默认 feature）
cargo build --workspace

# 排除 minicoding-desktop（默认 feature 不含 desktop，无需 Tauri 系统库）
cargo build --workspace --exclude minicoding-desktop
```

> **为何排除 minicoding-desktop**：`minicoding-desktop` 的 `desktop` feature 依赖 Tauri（需 webkit2gtk/glib 系统库），默认 feature 为空但 `[[bin]]` 设了 `required-features = ["desktop"]`，常规 `--all-features` 也不会触发 desktop 编译。CI 在独立的 `desktop` job 中安装系统依赖后单独编译（见 `.github/workflows/ci.yml`）。

首次构建会拉取 `tokio`/`reqwest`/`rmcp`/`ratatui` 等依赖，耗时 3–8 分钟（取决于网络与机器）。后续增量构建通常 < 30 秒。

### 4.2 单 crate 构建

```bash
# 仅构建 CLI
cargo build -p minicoding-cli

# 仅构建 core（最快，无重依赖）
cargo build -p minicoding-core

# 仅构建沙箱驱动
cargo build -p minicoding-sandbox

# 构建 CLI 并启用全部 feature
cargo build -p minicoding-cli --features full
```

### 4.3 构建产物位置

| Profile | 产物路径 | 用途 |
|---------|---------|------|
| debug（默认） | `target/debug/` | 开发调试 |
| release | `target/release/` | 发布构建 |
| 跨平台 | `target/<triple>/release/` | 交叉编译 |

主要二进制产物：

```
target/debug/
├── minicoding              # CLI 主二进制（minicoding-cli）
├── minicoding-tui          # TUI 前端
├── minicoding-server       # HTTP/SSE server
└── minicoding-desktop      # 桌面壳（仅启用 desktop feature 时生成）
```

验证构建：

```bash
cargo run -p minicoding-cli -- --help
cargo run -p minicoding-cli -- --version
```

### 4.4 构建配置（debug / release）

workspace 根 `Cargo.toml` 定义了 profile：

```toml
# Cargo.toml
[profile.release]
lto = "thin"          # thin LTO，平衡编译时间与运行时性能
codegen-units = 1     # 单编译单元，最大化优化
strip = true          # 剥离调试符号，减小二进制体积

[profile.dev]
debug = 1             # 行号调试信息（0=无，1=行号，2=完整）
```

- **开发期**：默认 `cargo build` 即 debug profile，含行号调试信息，编译快；
- **发布期**：`cargo build --release` 启用 thin LTO + 单 codegen-unit + strip，二进制更小更快，但编译时间显著增加（10–20 分钟）；
- **覆盖率测试**：使用专门的 `cargo-llvm-cov`，见 §10.3。

如需临时覆盖 profile（不修改 `Cargo.toml`），可在命令行指定：

```bash
# 覆盖 release 不做 LTO（加快编译）
cargo build --release --profile release \
    --config profile.release.lto=false \
    --config profile.release.codegen-units=16
```

---

## 5. Feature Flags 详解

### 5.1 特性门控设计

实现 crate 通过 cargo feature 按需启用，避免强制引入重依赖（见 `docs/modules.md` §0.4）。`minicoding-cli` 作为组装入口，通过 feature gate 控制可选能力：

```toml
# crates/minicoding-cli/Cargo.toml
[features]
default = ["memory", "sandbox"]
memory    = ["dep:minicoding-memory"]
hooks     = ["dep:minicoding-hooks"]
file-undo = ["dep:minicoding-journal"]
sandbox   = ["dep:minicoding-sandbox"]
mcp       = ["dep:minicoding-mcp"]
serve     = ["dep:minicoding-server"]
extensions = ["dep:minicoding-extension-sdk"]
web       = ["minicoding-tools/web"]              # web.fetch/web.search 工具
lsp       = ["serve", "minicoding-server/lsp"]    # LSP stdio 适配器
otel      = ["dep:tracing-opentelemetry", "dep:opentelemetry",
             "dep:opentelemetry_sdk", "dep:opentelemetry-otlp"]
full      = ["memory", "hooks", "file-undo", "sandbox", "mcp",
             "serve", "web", "lsp", "otel", "extensions"]
```

### 5.2 各 crate feature 一览

| Crate | Feature | 说明 | 引入的依赖 |
|-------|---------|------|-----------|
| `minicoding-cli` | `default` | 默认启用 | `memory` + `sandbox` |
| | `memory` | 长期/会话记忆 + AGENTS.md loader | `minicoding-memory` |
| | `hooks` | Hook 系统（10 类事件） | `minicoding-hooks` |
| | `file-undo` | FileChangeJournal + `/undo` | `minicoding-journal` |
| | `sandbox` | OS 沙箱驱动 | `minicoding-sandbox` |
| | `mcp` | MCP client/server | `minicoding-mcp`（含 `rmcp` 2.2） |
| | `serve` | HTTP/SSE server 模式 | `minicoding-server` |
| | `extensions` | 扩展系统（PromptPipeline + BundledExtensionHost） | `minicoding-extension-sdk` |
| | `web` | web.fetch/web.search 工具 | `minicoding-tools/web` → `reqwest` |
| | `lsp` | LSP stdio 适配器 | `minicoding-server/lsp` → `tower-lsp` |
| | `otel` | OTLP trace 导出 | `opentelemetry` + `opentelemetry-otlp` |
| | `full` | 启用全部上述 feature | 全部 |
| `minicoding-core` | `ts` | ts-rs 类型导出（M9 前端契约） | `ts-rs` |
| `minicoding-protocol` | `ts` | JSON-RPC DTO 导出 TypeScript 类型 | `ts-rs` + `minicoding-core/ts` |
| `minicoding-tools` | `web` | web 工具组 | `reqwest` + `url` |
| `minicoding-server` | `lsp` | LSP 适配器 | `tower-lsp` |
| | `acp` | ACP 适配器 | — |
| `minicoding-desktop` | `desktop` | Tauri 桌面壳 | `tauri` + 插件 |

### 5.3 常用 feature 组合

```bash
# 最小构建（仅 CLI 核心，无记忆无沙箱）
cargo build -p minicoding-cli --no-default-features

# 默认构建（memory + sandbox，日常开发推荐）
cargo build -p minicoding-cli

# 全功能构建（含 MCP/Hook/Server/LSP/OTel）
cargo build -p minicoding-cli --features full

# 仅构建 HTTP server 形态
cargo build -p minicoding-cli --no-default-features --features serve,memory,sandbox

# CI 标准构建（workspace 全量 + 全 feature，排除 desktop）
cargo build --workspace --exclude minicoding-desktop --all-features
```

### 5.4 平台条件依赖

平台重依赖通过 `[target.'cfg(target_os = "...")'.dependencies]` 隔离，非目标平台不编译（见 `AGENTS.md` §3.5）。以 `minicoding-sandbox` 为例：

```toml
# crates/minicoding-sandbox/Cargo.toml
[target.'cfg(target_os = "linux")'.dependencies]
landlock = "0.4"      # Linux Landlock LSM（内核 5.13+）
libc = "0.2"

[target.'cfg(target_os = "windows")'.dependencies]
windows-sys = { version = "0.59", features = [
    "Win32_Foundation",
    "Win32_System_JobObjects",
    "Win32_System_Threading",
    "Win32_System_Diagnostics_ToolHelp",
] }

# macOS：Seatbelt 通过 sandbox_init FFI 直连 libsystem，无外部 crate 依赖
```

`minicoding-sandbox::detect_driver()` 编译期按 `cfg!(target_os)` 选实现，运行期 `sandbox_run::landlock_available()` 探测内核支持。无可用硬隔离时返回 `NoopDriver`（来自 `minicoding-core`）并打 `warn`（见 `docs/tech-stack.md` §11）。

---

## 6. 前端构建（M9）

> **范围说明**：本节覆盖 `crates/minicoding-web/`（Web 前端）与 `crates/minicoding-desktop/`（Tauri 桌面壳）。技术选型见 `docs/tech-stack.md` §4.1，详细设计见 `docs/m9-design.md`。

### 6.1 Web 前端构建

#### 6.1.1 技术栈

`crates/minicoding-web/` 是独立 npm 项目（不属于 Cargo workspace），技术栈锁定见 `docs/m9-design.md` §3：

| 用途 | 选型 | 版本（package.json） |
|------|------|------|
| 框架 | React | 19.x |
| 语言 | TypeScript | 5.7+ |
| 构建 | Vite | 6.x |
| 样式 | Tailwind CSS | v4（Oxide 引擎） |
| 数据获取 | TanStack Query | 5.x |
| 客户端状态 | Zustand | 5.x |
| 组件库 | shadcn/ui（Radix UI） | latest |
| Schema 校验 | Zod（计划） | 4.x |

> **工具链一致性**：Vite (Rolldown) / Tailwind v4 (Oxide) 均为 Rust 实现，与后端工具链一致（见 `AGENTS.md` §8.7）。Lint 用 oxlint，格式化用 oxfmt（package.json 已配置脚本）。

#### 6.1.2 安装依赖

```bash
cd crates/minicoding-web

# npm（package-lock.json 已提交）
npm install

# 或 pnpm（m9-design.md 示例使用 pnpm）
pnpm install --frozen-lockfile
```

`package-lock.json` 提交到仓库，与 `Cargo.lock` 同等对待（见 `AGENTS.md` §8.7）。

#### 6.1.3 开发模式（HMR）

```bash
npm run dev
# Vite dev server 默认监听 http://localhost:5173
# 自动代理 /sessions 与 /health 到 http://localhost:8080（minicoding-server）
```

开发前需先启动后端 server：

```bash
# 项目根目录
cargo run -p minicoding-cli -- serve --http --bind 127.0.0.1:8080
```

Vite 配置（`vite.config.ts`）已内置代理，避免开发期 CORS 问题：

```typescript
// crates/minicoding-web/vite.config.ts
export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5173,
    proxy: {
      "/sessions": { target: "http://localhost:8080", changeOrigin: true },
      "/health":   { target: "http://localhost:8080", changeOrigin: true },
    },
  },
  build: { outDir: "dist", sourcemap: true },
});
```

#### 6.1.4 生产构建

```bash
npm run build
# 等价于：tsc -b && vite build
# 产物：crates/minicoding-web/dist/
```

#### 6.1.5 静态资源托管（单二进制部署）

`minicoding serve --web` 内置静态资源托管（`tower-http` 的 `ServeDir`，见 `Cargo.toml` workspace deps `tower-http = { features = ["cors", "fs"] }`）：

```bash
# 单二进制部署：后端 + 前端静态资源
cargo build --release -p minicoding-cli --features serve
./target/release/minicoding serve \
    --http \
    --bind 0.0.0.0:8080 \
    --web ./crates/minicoding-web/dist \
    --cors-origin "https://your-domain.com"
```

### 6.2 TypeScript 类型生成（ts-rs）

前端与后端的类型契约通过 `ts-rs` 自动生成，**不手写双份**（见 `AGENTS.md` §8.4）。

#### 6.2.1 生成命令

```bash
cd crates/minicoding-web
npm run gen-types
```

底层执行的命令（见 `package.json` `scripts.gen-types`）：

```bash
# 启用 minicoding-protocol 的 ts feature，通过 cargo test 触发 ts-rs 导出
cargo test -p minicoding-protocol --features ts -- --nocapture 2>&1 | tail -5
# 清理生成文件末尾空白
find src/api/generated -name '*.ts' -exec sed -i 's/[[:space:]]*$//' {} +
```

#### 6.2.2 产物位置

生成的 TypeScript 类型位于 `crates/minicoding-web/src/api/generated/`，**不手动编辑**（文件头标注 `// AUTO-GENERATED, DO NOT EDIT`）。当前已生成的类型包括：

- `Message.ts`、`ToolCall.ts`、`ToolResult.ts`、`Session.ts`
- `EventDto.ts`、`Command.ts`、`PermissionPrompt.ts`
- `SessionConfig.ts`、`ToolSchema.ts`、`SideEffect.ts`
- 等 30+ DTO 文件

#### 6.2.3 CI 校验

后端 DTO 变更后必须重新生成，CI 校验生成产物与 Rust 源一致（`git diff --exit-code`，见 `AGENTS.md` §8.4）：

```bash
npm run gen-types
git diff --exit-code src/api/generated/   # 应无 diff
```

### 6.3 桌面应用构建（Tauri 2.x）

#### 6.3.1 架构

`minicoding-desktop` 是 Tauri 2.x 桌面壳，通过 sidecar 进程启动 `minicoding-server`（见 `docs/m9-design.md` §6.1）。前端复用 `minicoding-web/dist/`，凭证复用 OS keyring（与 CLI `cred.rs` 共享 `KEYRING_SERVICE = "minicoding"`，C-04）。

#### 6.3.2 Linux 系统依赖

见 §2.2.3，需安装 `libwebkit2gtk-4.1-dev` 等。

#### 6.3.3 构建命令

```bash
# 编译 desktop feature（需先安装系统依赖）
cargo build -p minicoding-desktop --features desktop

# clippy 检查（CI 标准）
cargo clippy -p minicoding-desktop --features desktop -- -D warnings

# Tauri 开发模式（需 tauri-cli）
cargo tauri dev

# Tauri 打包（生成 .dmg / .msi / .AppImage）
cargo tauri build
```

#### 6.3.4 feature gate 说明

`minicoding-desktop` 的 `desktop` feature 是可选的，未启用时 `[[bin]]` 不编译（`required-features = ["desktop"]`）：

```toml
# crates/minicoding-desktop/Cargo.toml
[[bin]]
name = "minicoding-desktop"
path = "src/main.rs"
required-features = ["desktop"]

[features]
default = []
desktop = [
    "dep:tauri",
    "dep:tauri-plugin-shell",
    "dep:tauri-plugin-updater",
    "dep:tauri-plugin-global-shortcut",
]
```

Tauri 重依赖直接声明在 crate 内（而非 `workspace.dependencies`），避免未启用 feature 时 Cargo 解析整个 tauri 依赖树（见 `Cargo.toml` 注释）。

---

## 7. 跨平台构建

### 7.1 平台支持策略

沙箱与核心 Runtime 的多平台支持分阶段交付（见 `docs/tech-stack.md` §11「平台优先级」）：

| 阶段 | 平台支持 | 沙箱状态 |
|------|---------|---------|
| M0–M4（Linux 先行） | CI matrix 只跑 Linux | Linux: Landlock + libseccomp；macOS/Windows: 降级 NoopDriver |
| M5+（macOS 补齐） | 补齐 macOS CI matrix | macOS: Seatbelt（sandbox_init FFI） |
| M6+（Windows 补齐） | 补齐 Windows 实现 | Windows: Job Object + 受限令牌（windows-sys） |

当前 CI 三平台均有原生沙箱驱动（Linux Landlock + macOS Seatbelt + Windows Job Object），不再降级 NoopDriver（见 `.github/workflows/ci.yml` 注释）。

### 7.2 Linux 构建

#### 7.2.1 标准构建

```bash
# 安装系统依赖
sudo apt install libseccomp-dev

# 构建
cargo build --workspace --exclude minicoding-desktop
cargo test --workspace --exclude minicoding-desktop --all-features
```

#### 7.2.2 Landlock 内核要求

`landlock` crate 依赖 Linux 5.13+ 的 Landlock LSM。`minicoding-sandbox::detect_driver()` 运行时探测：

- 内核 5.13+：启用 Landlock + libseccomp，`is_hardened()` 返回 `true`；
- 内核 < 5.13：降级为 `NoopDriver`，打 `warn` 日志，仅应用层权限生效；
- 检查命令：`uname -r` 与 `minicoding doctor --security`。

这是设计内的 fail-open 降级，不阻塞编译与运行（见 `docs/getting-started.md` §1.5）。

#### 7.2.3 aarch64 交叉编译

```bash
# 安装交叉工具链
sudo apt install gcc-aarch64-linux-gnu

# 设置 linker
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

# 添加 target
rustup target add aarch64-unknown-linux-gnu

# 构建
cargo build --release --target aarch64-unknown-linux-gnu \
    --workspace --exclude minicoding-desktop
```

### 7.3 macOS 构建

```bash
# 无需额外系统包（Seatbelt 通过 FFI 直连 libsystem）
cargo build --workspace --exclude minicoding-desktop
cargo test --workspace --exclude minicoding-desktop --all-features
```

- macOS 12+ 由 `sandbox-run` 生成 profile 并 `apply_sandbox`；
- macOS 通用二进制（x86_64 + aarch64）需分别构建后 `lipo` 合并。

### 7.4 Windows 构建

```bash
# 无需额外系统包（windows-sys 使用系统 API）
cargo build --workspace --exclude minicoding-desktop
cargo test --workspace --exclude minicoding-desktop --all-features
```

- Windows 沙箱用 Job Object + 受限令牌（`windows-sys` crate）；
- 产物为 `.exe`，发布时打包为 `.zip`（见 §8.2）。

### 7.5 平台条件依赖说明

平台重依赖的编译期隔离见 §5.4。关键点：

- `minicoding-core` 依赖必须是"轻量 + 无平台/网络"（仅 `tokio`/`serde`/`tracing`/`thiserror`/`uuid`/`time`/`camino`/`trait-variant`，见 `docs/modules.md` §1.4）；
- `reqwest`/`landlock`/`libseccomp`/`rmcp`/`ratatui`/`windows-sys`/`tauri` 只在对应实现 crate 引入；
- 非 Linux 平台 `cargo build -p minicoding-sandbox` 仍可通过（`landlock`/`libseccomp` 通过 target cfg 条件引入，不编译）。

---

## 8. 发布构建

### 8.1 cargo-dist 配置

项目使用 [cargo-dist](https://opensource.axo.dev/cargo-dist/) 管理跨平台二进制发布（见 `docs/features.md` Q-08/Q-09）。配置文件 `cargo-dist.toml`：

```toml
# cargo-dist.toml
[dist]
# 目标三元组（三平台 5 个 target）
targets = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
]

dist = false
ci = ["github"]
installers = ["shell", "powershell", "homebrew", "scoop"]
publish-jobs = ["homebrew", "scoop"]

# Homebrew tap 仓库
tap = "minicoding/homebrew-tap"
# Scoop bucket 仓库
scoop = "minicoding/scoop-bucket"

cargo-dist-version = "0.25.0"
rust-toolchain-version = "1.99.0"

create-release = true
pr-run-mode = "plan"
```

**设计要点**：

- **不含 `minicoding-desktop`**：Tauri 需平台 GUI 依赖，单独发布 `.dmg`/`.msi`/`.AppImage`（见 `docs/m9-design.md` §8.2）；
- **5 个 target** 覆盖 Linux（x86_64/aarch64）、macOS（Intel/Apple Silicon）、Windows（x86_64）；
- **4 种安装器**：shell（Linux/macOS）、powershell（Windows）、homebrew（macOS）、scoop（Windows）；
- **发布渠道**：Homebrew tap + Scoop bucket + `cargo install`（见 `docs/features.md` Q-09）。

### 8.2 跨平台二进制打包

#### 8.2.1 本地触发 cargo-dist

```bash
# 安装 cargo-dist
cargo install cargo-dist --version 0.25.0

# 构建所有 target 的发布产物
cargo dist build

# 仅计划（不实际构建）
cargo dist plan
```

#### 8.2.2 GitHub Releases 自动构建

推送 `v*` tag 触发 `.github/workflows/release.yml`：

```bash
git tag v0.1.0
git push origin v0.1.0
```

release workflow 执行：

1. **build job**：5 个 target 并行构建（`cargo build --release --target <triple> --workspace --exclude minicoding-desktop`）；
2. **打包**：二进制 + LICENSE + README.md + AGENTS.md + cargo-dist.toml + 安装脚本（`install.sh` / `install.ps1`）；
3. **release job**：创建 GitHub Release，上传所有 `.tar.gz` / `.zip` 产物。

aarch64-linux 交叉编译需安装 `gcc-aarch64-linux-gnu` 并设置 `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER`（见 `.github/workflows/release.yml`）。

#### 8.2.3 产物矩阵

| 平台 | 架构 | 产物文件名 |
|------|------|-----------|
| Linux | x86_64 | `minicoding-<ver>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | aarch64 | `minicoding-<ver>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | x86_64 | `minicoding-<ver>-x86_64-apple-darwin.tar.gz` |
| macOS | aarch64 | `minicoding-<ver>-aarch64-apple-darwin.tar.gz` |
| Windows | x86_64 | `minicoding-<ver>-x86_64-pc-windows-msvc.zip` |

### 8.3 安装方式

发布后用户可通过以下方式安装（见 `.github/workflows/release.yml` body）：

```bash
# 方式一：shell installer（Linux/macOS）
curl -LsSf https://github.com/minicoding/minicoding-rs/releases/download/v0.1.0/minicoding-0.1.0-x86_64-unknown-linux-gnu.tar.gz | tar xz
./minicoding-*/install.sh

# 方式二：PowerShell（Windows）
Invoke-WebRequest -Uri "https://github.com/minicoding/minicoding-rs/releases/download/v0.1.0/minicoding-0.1.0-x86_64-pc-windows-msvc.zip" -OutFile "minicoding.zip"
Expand-Archive minicoding.zip
.\minicoding-*/install.ps1

# 方式三：cargo install
cargo install minicoding

# 方式四：Homebrew（macOS）
brew install minicoding/tap/minicoding

# 方式五：Scoop（Windows）
scoop bucket add minicoding https://github.com/minicoding/scoop-bucket
scoop install minicoding
```

---

## 9. 构建优化

### 9.1 增量编译

项目已通过以下设计优化增量编译：

- **core 轻量**：`minicoding-core` 无平台/网络重依赖，编译快、测试快（见 `docs/modules.md` §1.4）；
- **feature gate 隔离**：重依赖（`reqwest`/`rmcp`/`landlock`/`tauri`）只在对应 crate 引入，未启用 feature 时不编译；
- **workspace 统一版本**：`[workspace.dependencies]` 集中管理依赖版本，避免同 crate 多版本重复编译（见 `Cargo.toml`）。

开发期建议：

```bash
# 只构建变更的 crate 及其依赖
cargo build -p minicoding-cli

# 利用 sccache（见 §9.2）
export RUSTC_WRAPPER=sccache
```

### 9.2 sccache

[sccache](https://github.com/mozilla/sccache) 缓存编译产物，加速重复编译（CI 与本地均受益）：

```bash
# 安装
cargo install sccache

# 启用（写入 ~/.cargo/config.toml 或环境变量）
export RUSTC_WRAPPER=sccache

# 或持久化到 ~/.cargo/config.toml
# [build]
# rustc-wrapper = "sccache"

# 查看缓存统计
sccache --show-stats
```

> **CI 注意**：`.github/workflows/ci.yml` 使用 `Swatinem/rust-cache@v2` 缓存 `target/` 目录，与 sccache 互补。本地开发建议同时启用两者。

### 9.3 mold linker（Linux）

[mold](https://github.com/rui314/mold) 是高性能 linker，显著加快 release 构建（LTO 链接阶段）：

```bash
# 安装
sudo apt install mold

# 配置 ~/.cargo/config.toml
# [target.x86_64-unknown-linux-gnu]
# linker = "clang"
# rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

需配合 `clang` 作为 linker 驱动（`gcc` 不支持 `-fuse-ld=mold`）。macOS 用系统 `ld`，Windows 用 `link.exe`，无需 mold。

### 9.4 profile 调优

workspace 默认 profile（见 §4.4）已针对发布优化。按场景调优建议：

| 场景 | 调优 | 命令 |
|------|------|------|
| 开发调试 | 保留行号调试信息，关闭优化 | `cargo build`（默认 dev） |
| 快速 release 验证 | 关闭 LTO，增大 codegen-units | `cargo build --release --config profile.release.lto=false --config profile.release.codegen-units=16` |
| 正式发布 | thin LTO + 单 codegen-unit + strip | `cargo build --release`（默认） |
| 覆盖率 | 专门 profile | `cargo llvm-cov`（见 §10.3） |

---

## 10. 测试构建

### 10.1 单元测试

单元测试与源码同文件（`#[cfg(test)] mod tests { ... }`），异步测试用 `#[tokio::test]`（见 `AGENTS.md` §2.8）：

```bash
# 全量单元测试
cargo test --workspace --exclude minicoding-desktop --all-features

# 单 crate 测试
cargo test -p minicoding-core

# 单测试函数
cargo test -p minicoding-policy -- builtin::tests
```

测试不连真实服务（`AGENTS.md` §5.4）：

- LLM API 测试用 `wiremock`/`httpmock` 模拟；
- MCP server 测试用本地 mock stdio process；
- 沙箱测试用 `tempfile` 临时目录；
- CI 注入 mock 凭证：`OPENAI_API_KEY=sk-test-ci-mock-not-real`。

### 10.2 集成测试

集成测试放 `tests/` 目录，按场景命名（`agent_loop.rs`/`compression.rs`/`sandbox.rs`）。跨 crate 共享测试工具放 `crates/minicoding-core/tests/common/`（见 `AGENTS.md` §2.8）。

```bash
# 运行集成测试（仅 tests/ 目录）
cargo test --workspace --test '*' --all-features

# 运行特定集成测试文件
cargo test -p minicoding-core --test agent_loop
```

### 10.3 覆盖率工具（cargo-llvm-cov）

覆盖率目标 ≥80%（`AGENTS.md` §2.8），CI 用 `cargo-llvm-cov` 检查：

```bash
# 安装
cargo install cargo-llvm-cov

# 生成覆盖率报告（HTML）
cargo llvm-cov --workspace --all-features --html

# CI 标准：排除前端层，行覆盖率门槛 80%
cargo llvm-cov --workspace \
    --exclude minicoding-desktop \
    --exclude minicoding-tui \
    --exclude minicoding-cli \
    --exclude minicoding-server \
    --all-features \
    --fail-under-lines 80
```

**排除项说明**（见 `.github/workflows/ci.yml` coverage job 注释）：

- `minicoding-tui`：终端渲染，需 TTY 仿真；
- `minicoding-cli`：入口 bin，集成测试覆盖；
- `minicoding-server`：HTTP/SSE 前端层，集成测试覆盖；
- `minicoding-desktop`：Tauri 桌面壳，需 GUI 运行时。

### 10.4 属性测试与基准测试

```bash
# 属性测试（proptest，Message JSON roundtrip + path sandbox 不变量）
cargo test --features proptest -p minicoding-core

# 性能基准（criterion，压缩管道 100/500/1000 消息基准）
cargo bench -p minicoding-context
```

详见 `docs/tech-stack.md` §10。

---

## 11. 质量检查工具链

### 11.1 cargo fmt

```bash
# 检查格式（CI 标准）
cargo fmt --all -- --check

# 自动格式化
cargo fmt --all
```

### 11.2 cargo clippy

每个 crate `lib.rs` 顶部 `#![deny(clippy::all, clippy::pedantic)]` 起步（`AGENTS.md` §2.9）：

```bash
# CI 标准：deny warnings，全 feature，排除 desktop
cargo clippy --workspace --exclude minicoding-desktop --all-targets --all-features -- -D warnings

# desktop 单独检查（需先安装系统依赖）
cargo clippy -p minicoding-desktop --features desktop -- -D warnings
```

例外用 `#[allow(clippy::xxx)]` + 紧跟一行注释说明理由，不用 `#![allow(...)]` 全局放松。

### 11.3 cargo audit

漏洞检测，CI 默认 vulnerabilities 失败、unmaintained/yanked/notices 仅警告：

```bash
# 安装
cargo install cargo-audit

# 检查
cargo audit
```

不使用 `--deny warnings`：部分传递依赖可能触发 unmaintained 警告（如历史上的 `number_prefix`），CI 不应因 unmaintained 警告阻断（见 `.github/workflows/ci.yml` audit job 注释）。

### 11.4 cargo deny

许可证 + 来源 + 重复依赖检查，配置见 `deny.toml`：

```bash
# 安装
cargo install cargo-deny

# 检查全部维度
cargo deny check advisories licenses bans sources
```

`deny.toml` 关键配置：

```toml
# deny.toml
[graph]
# 排除 minicoding-desktop：desktop feature 依赖 Tauri，含非白名单许可证
exclude = ["minicoding-desktop"]
all-features = true

[licenses]
# 许可证白名单（AGENTS.md §2.7：仅 MIT/Apache-2.0/BSD/ISC + 传递依赖必需）
allow = [
    "MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause", "BSD-3-Clause", "ISC",
    "AGPL-3.0-only",   # 项目自身 crate
    "BSL-1.0", "OpenSSL", "Unicode-DFS-2016", "Unicode-3.0",
    "Zlib", "CC0-1.0", "Unlicense", "CDLA-Permissive-2.0",
]

[sources]
# 仅允许 crates.io
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
```

### 11.5 typos（拼写检查）

```bash
# 安装
cargo install typos-cli

# 检查
typos
```

### 11.6 pre-commit hooks（可选）

项目未强制要求 pre-commit hook，但建议本地配置以提前发现问题：

```bash
# .git/hooks/pre-commit（示例）
#!/bin/bash
set -e
cargo fmt --all -- --check
cargo clippy --workspace --exclude minicoding-desktop --all-features -- -D warnings
typos
```

```bash
chmod +x .git/hooks/pre-commit
```

---

## 12. CI/CD 流水线

### 12.1 CI 门禁（`.github/workflows/ci.yml`）

CI 共 9 道门禁，全部通过方可合并 PR（见 `AGENTS.md` §6.3）：

| Job | 名称 | 平台 | 说明 |
|-----|------|------|------|
| `fmt` | cargo fmt --check | ubuntu | 格式检查 |
| `clippy` | cargo clippy -D warnings | ubuntu | 全 feature + 排除 desktop |
| `test` | cargo test | ubuntu | 全 feature + 排除 desktop |
| `coverage` | cargo llvm-cov (≥80%) | ubuntu | 排除前端层，行覆盖率门槛 80% |
| `audit` | cargo audit | ubuntu | 漏洞检测 |
| `deny` | cargo deny | ubuntu | 许可证 + 来源 + bans |
| `typos` | typos | ubuntu | 拼写检查 |
| `cross-platform` | cargo test | macos + windows | 跨平台编译 + 单测 matrix |
| `desktop` | cargo build desktop feature | ubuntu | Tauri 桌面壳编译 + clippy |

**环境变量**：

```yaml
env:
  CARGO_TERM_COLOR: always
  RUSTFLAGS: "-D warnings"           # 警告视为错误
```

**测试凭证**：所有 test job 注入 `OPENAI_API_KEY: sk-test-ci-mock-not-real`（mock 凭证，不连真实服务，见 `AGENTS.md` §5.4）。

### 12.2 平台策略

| 平台 | CI 覆盖 | 沙箱驱动 |
|------|---------|---------|
| Linux | 完整门禁（fmt/clippy/test/coverage/audit/deny/typos）+ Landlock 沙箱拒绝语义测试 | Landlock + libseccomp |
| macOS | 编译 + 单测 matrix | Seatbelt（sandbox_init FFI） |
| Windows | 编译 + 单测 matrix | Job Object（windows-sys FFI） |

三平台均有原生沙箱驱动，不再降级 NoopDriver（见 `.github/workflows/ci.yml` 注释）。

### 12.3 desktop job 依赖

`desktop` job 在 Linux 上安装 Tauri 系统依赖后单独编译（见 §6.3.2）：

```yaml
- name: Install Tauri system dependencies
  run: |
    sudo apt-get update
    sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
        libglib2.0-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev
- name: cargo build desktop
  run: cargo build -p minicoding-desktop --features desktop
- name: cargo clippy desktop
  run: cargo clippy -p minicoding-desktop --features desktop -- -D warnings
```

仅编译不运行（需 GUI 运行时），验证 desktop feature 代码正确性。

### 12.4 Release 流水线（`.github/workflows/release.yml`）

见 §8.2.2，推送 `v*` tag 触发，5 个 target 并行构建 + 打包 + 发布 GitHub Release。

---

## 13. 常见构建问题排查

### 13.1 Linux libseccomp 链接错误

**现象**：`cargo build` 报 `libseccomp` 链接错误。

**原因**：Linux 未安装 `libseccomp-dev`。

**解决**：

```bash
sudo apt install libseccomp-dev
```

### 13.2 内核 < 5.13 不支持 Landlock

**现象**：运行时 `minicoding doctor --security` 报沙箱降级。

**原因**：Linux 内核 < 5.13，无 Landlock LSM。

**解决**：这是设计内的 fail-open 降级，不阻塞编译与运行（见 `docs/getting-started.md` §1.5）。

- 检查：`uname -r`；
- 依赖容器/WSL2 做硬隔离，或显式选择 `--preset external-sandbox`（声明依赖外部容器隔离，见 `docs/security.md` §8.1）。

### 13.3 Tauri 桌面构建失败

**现象**：`cargo build -p minicoding-desktop --features desktop` 报 webkit2gtk 找不到。

**原因**：Linux 未安装 Tauri 系统依赖。

**解决**：见 §2.2.3 安装 `libwebkit2gtk-4.1-dev` 等。

### 13.4 clippy -D warnings 失败

**现象**：CI clippy job 失败。

**原因**：代码违反 `clippy::all` + `clippy::pedantic`（`AGENTS.md` §2.9）。

**解决**：按提示修复，不全局 `#![allow(...)]`。例外用 `#[allow(clippy::xxx)]` + 紧跟注释说明理由。

### 13.5 cargo audit 报漏洞

**现象**：`cargo audit` 报 RUSTSEC 条目。

**解决**：

```bash
# 升级补丁版本
cargo update

# 若需升级大版本，审查 changelog 后单独 PR
```

CI 阻塞合并（见 `AGENTS.md` §6.3）。

### 13.6 cargo deny 许可证失败

**现象**：`cargo deny check licenses` 报非白名单许可证。

**解决**：

1. 检查是否引入了非白名单许可证的依赖；
2. 若是传递依赖必需（如 `OpenSSL`/`Zlib`/`Unicode-3.0`），在 `deny.toml` `[licenses] allow` 中追加并注释说明理由；
3. 项目自身 crate 用 `AGPL-3.0-only`（已在白名单）。

### 13.7 TypeScript 类型生成不一致

**现象**：CI 校验 `git diff --exit-code src/api/generated/` 失败。

**原因**：后端 DTO 变更后未重新生成 TypeScript 类型。

**解决**：

```bash
cd crates/minicoding-web
npm run gen-types
git add src/api/generated/
git commit -m "feat(web): 同步生成 TypeScript 类型"
```

### 13.8 测试因凭证缺失失败

**现象**：本地 `cargo test` 报 `OPENAI_API_KEY` 未设置。

**解决**：测试不连真实服务，注入 mock 凭证即可（见 `AGENTS.md` §5.4）：

```bash
export OPENAI_API_KEY=sk-test-mock-not-real
cargo test --workspace --all-features
```

### 13.9 非 TTY 下副作用工具被拒

**现象**：`cargo test` 或 `minicoding exec` 在非 TTY 环境副作用工具被拒。

**原因**：`NonInteractivePrompter` 默认 `deny`（`docs/security.md` §2.1）。

**解决**：显式 `--allow` 或改 `permission.non_tty_strategy` 配置。

### 13.10 Windows 沙箱成熟度低

**现象**：Windows 上 `is_hardened()` 返回 `false`。

**说明**：Windows 缺乏 macOS Seatbelt / Linux Landlock 这样成熟的内核级 MAC 框架（见 `docs/security.md` §12）。建议在 WSL2/容器内运行，或等待 M6+ 补齐受限令牌 + Job Object 实现。

---

## 14. Docker 构建

> **适用场景**：CI/容器化部署、`ExternalSandbox` 预设（声明依赖外部容器隔离，见 `docs/security.md` §8.1）。项目仓库当前未提交 Dockerfile，以下为推荐实践。

### 14.1 为什么用 Docker

- **CI 隔离**：CI runner 内运行 `minicoding` 时，外层容器提供隔离，叠加本进程沙箱既冗余又易因容器权限不足失败（Landlock 需 `CAP_SYS_ADMIN` 或无 seccomp 限制）；
- **`ExternalSandbox` 预设**：`minicoding --preset external-sandbox` 声明依赖外部容器隔离，`SandboxDriver::is_hardened()` 返回 `false`，`detect_driver()` 返回 `NoopDriver`，仅应用层权限生效（见 `docs/security.md` §8.1）。

### 14.2 推荐 Dockerfile

```dockerfile
# Dockerfile（推荐实践，非仓库提交文件）
FROM rust:1.99-bookworm AS builder

# 安装 libseccomp（若容器内需启用沙箱）
RUN apt-get update && apt-get install -y libseccomp-dev git

WORKDIR /app
COPY . .
COPY rust-toolchain.toml ./

# 构建 release（启用 serve feature 用于容器化部署）
RUN cargo build --release -p minicoding-cli --features serve,sandbox,memory

# 运行阶段
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libseccomp2 ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/minicoding /usr/local/bin/
COPY --from=builder /app/target/release/minicoding-server /usr/local/bin/

EXPOSE 8080
ENTRYPOINT ["minicoding"]
CMD ["serve", "--http", "--bind", "0.0.0.0:8080", "--preset", "external-sandbox"]
```

### 14.3 构建与运行

```bash
# 构建镜像
docker build -t minicoding:latest .

# 运行 server（ExternalSandbox 预设，依赖容器隔离）
docker run -p 8080:8080 \
    -e OPENAI_API_KEY=sk-... \
    minicoding:latest

# 容器内无需 Landlock（容器已隔离），用 external-sandbox 预设
docker run -p 8080:8080 \
    -e OPENAI_API_KEY=sk-... \
    minicoding:latest serve --preset external-sandbox
```

### 14.4 容器内沙箱注意事项

- **Landlock 在容器内可能不可用**：Docker 默认 seccomp profile 可能限制 Landlock 系统调用，需 `--security-opt seccomp=unconfined` 或自定义 profile；
- **推荐 `external-sandbox` 预设**：容器本身提供隔离，本进程沙箱降级为 `NoopDriver` 是正确行为，避免冗余；
- **`doctor --security` 如实报告**：`is_hardened() = false` 并建议"依赖外部容器隔离"。

---

## 15. 附录

### 15.1 构建相关配置文件索引

| 文件 | 用途 | 关联章节 |
|------|------|---------|
| `Cargo.toml` | workspace 根配置（成员、依赖版本、profile） | §4、§5、§9 |
| `Cargo.lock` | 依赖锁定（已提交） | §3 |
| `rust-toolchain.toml` | 工具链固定（nightly + 组件） | §2.1 |
| `deny.toml` | cargo-deny 配置（许可证/来源/bans） | §11.4 |
| `cargo-dist.toml` | 发布构建配置（5 target + 4 installer） | §8 |
| `.github/workflows/ci.yml` | CI 门禁（9 job） | §12 |
| `.github/workflows/release.yml` | Release 自动构建 | §8.2.2 |
| `crates/minicoding-web/package.json` | Web 前端依赖与脚本 | §6.1 |
| `crates/minicoding-web/vite.config.ts` | Vite 构建配置 | §6.1.3 |
| `crates/minicoding-web/tsconfig.json` | TypeScript 编译配置 | §6.1 |
| `crates/minicoding-desktop/Cargo.toml` | Tauri 桌面 crate 配置 | §6.3 |
| `crates/minicoding-cli/Cargo.toml` | CLI feature gate 定义 | §5 |
| `crates/minicoding-sandbox/Cargo.toml` | 平台条件依赖示例 | §5.4 |

### 15.2 常用构建命令速查

```bash
# === 开发 ===
cargo build --workspace --exclude minicoding-desktop     # 全量构建
cargo build -p minicoding-cli                            # 单 crate
cargo run -p minicoding-cli -- --help                    # 运行 CLI
cargo test --workspace --exclude minicoding-desktop --all-features  # 全量测试

# === 质量检查 ===
cargo fmt --all -- --check                               # 格式检查
cargo clippy --workspace --exclude minicoding-desktop --all-targets --all-features -- -D warnings
cargo audit                                              # 漏洞检测
cargo deny check advisories licenses bans sources        # 许可证/来源
typos                                                    # 拼写检查
cargo llvm-cov --workspace --exclude minicoding-desktop --all-features --fail-under-lines 80

# === 前端 ===
cd crates/minicoding-web
npm install
npm run dev                                              # 开发 HMR
npm run build                                            # 生产构建
npm run gen-types                                        # 生成 TS 类型

# === 桌面 ===
cargo build -p minicoding-desktop --features desktop     # 编译
cargo clippy -p minicoding-desktop --features desktop -- -D warnings
cargo tauri dev                                          # Tauri 开发
cargo tauri build                                        # Tauri 打包

# === 发布 ===
cargo dist build                                         # cargo-dist 构建
git tag v0.1.0 && git push origin v0.1.0                # 触发 Release

# === 交叉编译 ===
cargo build --release --target aarch64-unknown-linux-gnu --workspace --exclude minicoding-desktop
```

### 15.3 关键文档引用

- 技术选型与系统依赖：`docs/tech-stack.md` §1、§11
- crate 结构与特性门控：`docs/modules.md` §0.1、§0.4
- 快速上手：`docs/getting-started.md` §1
- Web/桌面设计：`docs/m9-design.md` §8
- 安全模型与沙箱：`docs/security.md` §8
- 编码规范与依赖治理：`AGENTS.md` §2、§5
- CI 配置：`.github/workflows/ci.yml`
- Release 配置：`.github/workflows/release.yml`、`cargo-dist.toml`
