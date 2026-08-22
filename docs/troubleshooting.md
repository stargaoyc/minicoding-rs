# minicoding-rs 问题排查手册

> **文档性质**：本文是 `minicoding-rs` 的「遇到的问题与解决方案」积累文档，记录项目开发与运行过程中实际遇到的构建、CI、运行时、测试、权限、MCP/Hook、前端、性能、调试等典型问题及其根因、解决方案与预防措施。
>
> **组织方式**：按问题类别分章节，章节内每个问题统一采用「问题描述 → 原因分析 → 解决方案 → 预防措施」四段式结构，便于检索与复盘。章节编号连续（§1–§11）。
>
> **适用范围**：18 crate workspace（含 `minicoding-desktop` Tauri 桌面壳与 `minicoding-web` React 前端），跨平台 Linux/macOS/Windows。
>
> **配套文档**：架构与设计见 `docs/design.md`；技术选型与平台依赖见 `docs/tech-stack.md`；编码约束见 `AGENTS.md`；运行时大模型约束见 `docs/rules.md`；快速入门与基础排查见 `docs/getting-started.md` §1.5。本文不重复上述内容，只聚焦「实际问题与解法」。

---

## 1. 文档说明

### 1.1 文档目的

- **故障速查**：开发或运行中遇到报错时，按章节定位同类问题与已验证的解法，避免重复踩坑。
- **知识沉淀**：把分散在 `ci.yml`/`deny.toml`/`pre-commit`/源码注释/PR 讨论中的「为什么这么配」集中成可检索的知识库。
- **新成员 onboarding**：配合 `docs/getting-started.md` 使用——入门指南解决「怎么跑起来」，本文解决「跑不起来怎么办」。

### 1.2 组织方式

| 章节 | 类别 | 典型读者 |
|------|------|---------|
| §2 | 构建与编译问题 | 所有开发者、CI 维护者 |
| §3 | CI/CD 问题 | CI 维护者、PR 提交者 |
| §4 | 运行时问题 | 终端用户、集成方 |
| §5 | 测试与覆盖率问题 | 所有开发者 |
| §6 | 权限与安全问题 | 安全审查者、策略配置者 |
| §7 | MCP 与 Hook 问题 | MCP/Hook 用户、扩展作者 |
| §8 | 前端与桌面问题 | M9 前端/桌面开发者 |
| §9 | 性能问题 | 性能调优、生产部署 |
| §10 | 调试技巧 | 所有开发者 |
| §11 | 问题反馈渠道 | 所有用户 |

### 1.3 阅读约定

- 命令中 `$ ` 前缀表示在用户 shell 中执行，`# ` 前缀表示需要 `sudo`；
- 路径默认相对仓库根 `/home/star/projects/minicoding-rs/`（或你的本地 clone 根）；
- 引用 `docs/xxx.md` 均为仓库内相对路径；引用 `AGENTS.md` §x / `rules.md` C-x 为约束编号；
- 所有解决方案均已在项目中验证，未验证的「建议」会显式标注。

---

## 2. 构建与编译问题

### 2.2 glib-sys 构建失败（Tauri 桌面 CI）

#### 问题描述

在 Linux 上执行 `cargo build -p minicoding-desktop --features desktop` 时报错：

```text
error: failed to run custom build command for `glib-sys`
  = note: pkg-config exited with status code 1
  = note: PKG_CONFIG_ALLOW_SYSTEM_CFLAGS=1 pkg-config --libs --cflags glib-2.0 gobject-2.0
          Package glib-2.0 was not found in the pkg-config search path.
```

或 `webkit2gtk-4.1`、`libsoup-3.0`、`javascriptcoregtk-4.1` 类似未找到。

#### 原因分析

`minicoding-desktop` 的 `desktop` feature 依赖 Tauri 2.x，Tauri 在 Linux 通过系统 webview（`webkit2gtk`）渲染，需要一组 GTK/GLib/Soup/JavaScriptCore 系统库的开发头文件。这些是 Tauri 的硬性系统依赖（见 `docs/tech-stack.md` §4.1），与 `AGENTS.md` §3.5「重依赖通过 feature gate 或 target cfg 隔离在对应实现 crate」一致——`desktop` feature 默认关闭，不在常规 `--all-features` 中编译。

CI 中 `clippy`/`test`/`coverage`/`cross-platform` job 均 `--exclude minicoding-desktop`，只在 `desktop` job 单独安装依赖后编译（见 `.github/workflows/ci.yml`）。

#### 解决方案

安装 Tauri Linux 系统依赖（与 `ci.yml` 的 `desktop` job 一致）：

```bash
sudo apt-get update
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev \
  libgtk-3-dev \
  libglib2.0-dev \
  libsoup-3.0-dev \
  libjavascriptcoregtk-4.1-dev
```

Fedora 等效包名（供参考）：

```bash
sudo dnf install -y webkit2gtk4.1-devel gtk3-devel glib2-devel libsoup3-devel javascriptcoregtk4.1-devel
```

重新构建：

```bash
cargo build -p minicoding-desktop --features desktop
```

#### 预防措施

- 桌面开发是 M9 可选里程碑，**非桌面开发者无需安装这些库**——常规 `cargo build --workspace --exclude minicoding-desktop` 不需要；
- 本地开发 `minicoding-desktop` 前，先确认上述包已安装；
- CI 的 `desktop` job 已固化安装步骤，PR 改动 `minicoding-desktop` 时 CI 会自动验证；
- 不要把 `desktop` feature 加入 workspace 默认 `--all-features` 编译，否则会拖慢全员构建（见 §2.3）。

---

### 2.3 Tauri 依赖导致 workspace 编译变慢

#### 问题描述

启用 `desktop` feature 后，`cargo build --workspace --all-features` 编译时间从约 5 分钟暴增到 12–15 分钟，且 `cargo check` 也明显变慢。开发者反馈「只想改个 CLI，却要等 Tauri 全家桶编译完」。

#### 原因分析

Tauri 2.x 传递引入大量 GUI 依赖（`wry`/`tao`/`webkit2gtk`/`glib`/`gtk`/`soup`/`javascriptcore`/`windows`/`objc2` 等），且部分依赖跨平台编译路径复杂。`--all-features` 会触发 `desktop` feature 的全部传递依赖编译。

项目通过 feature gate 把 `desktop` 隔离在 `minicoding-desktop` 单 crate（见 `AGENTS.md` §3.5），但 `--all-features` 仍会启用它。

#### 解决方案

**常规开发不使用 `--all-features`**，按需指定：

```bash
# 只开发 CLI/TUI/SDK，不碰桌面
cargo build --workspace --exclude minicoding-desktop
cargo clippy --workspace --exclude minicoding-desktop --all-targets --all-features -- -D warnings
cargo test --workspace --exclude minicoding-desktop --all-features

# 只开发桌面
cargo build -p minicoding-desktop --features desktop
```

CI 的 `clippy`/`test`/`coverage`/`cross-platform` job 已统一 `--exclude minicoding-desktop`（见 `.github/workflows/ci.yml`），仅 `desktop` job 单独编译桌面。

`cargo deny` 也通过 `deny.toml` 的 `[graph] exclude = ["minicoding-desktop"]` 排除桌面 crate 的依赖治理（Tauri 传递依赖许可证不在白名单中，desktop 为可选 feature 不影响核心 crate）。

#### 预防措施

- 本地 `cargo watch` / IDE 后台检查时，配置 `--exclude minicoding-desktop`；
- pre-commit hook 已 `--exclude minicoding-desktop`（见 `scripts/git-hooks/pre-commit`），避免提交时全量编译；
- 仅在改 `minicoding-desktop` 时显式 `cargo build -p minicoding-desktop --features desktop`；
- 不要为了「省事」用 `cargo build --workspace --all-features` 做日常开发。

---

### 2.4 edition 2024 的 set_var/remove_var 需要 unsafe

#### 问题描述

升级到 Rust edition 2024 后，原本正常的 `std::env::set_var` / `std::env::remove_var` 调用编译报错：

```text
error[E0133]: call to unsafe function `std::env::set_var` is unsafe and requires unsafe block
  --> crates/minicoding-core/src/paths.rs:100:13
   |
100 |             unsafe { env::set_var(key, value) };
   |             ^^^^^^^ call to unsafe function
   |
   = note: consult the function's documentation for information about how to avoid undefined behavior
```

`clippy::pedantic` 还会额外报 `clippy::undocumented_unsafe_blocks`。

#### 原因分析

Rust 2024 edition 把 `std::env::set_var` / `remove_var` / `set_current_dir` 等修改进程级全局状态的函数标记为 `unsafe`，原因是这些操作在多线程下非线程安全（`getenv` 在 Unix 上不线程安全，可能读到撕裂值或释放后内存）。这是 edition 2024 的破坏性变更之一（见 `AGENTS.md` §2.6「unsafe 默认禁用，必须使用时需 `// SAFETY:` 注释」）。

项目在以下场景使用：

- `crates/minicoding-sandbox/src/hardening.rs`：启动早期清除 `LD_*` 环境变量（防动态链接器注入，C-04）；
- `crates/minicoding-core/src/paths.rs`：测试用 `EnvGuard` 切换 `MINICODING_HOME`；
- `crates/minicoding-core/src/otel.rs`：测试隔离 `OTEL_TRACES_SAMPLER`；
- `crates/minicoding-cli/src/cred.rs`：测试隔离 `MINICODING_HOME`；
- `crates/minicoding-mcp/src/client/rmcp.rs`：测试 MCP env 展开。

#### 解决方案

**生产代码**（`hardening.rs`）：在 main 单线程阶段调用，包裹 `unsafe` 块并写 `// SAFETY:` 注释：

```rust
// SAFETY: `remove_var` 在 Rust 2024 标记为 unsafe 是因为多线程下修改环境
// 非线程安全。此处仅在 minicoding 启动早期（main 单线程阶段）调用，且
// 清除 `LD_*` 是一次性操作，不与并发读 env 的代码交错。
unsafe {
    std::env::remove_var(&k);
}
```

**测试代码**（`paths.rs`/`otel.rs`/`cred.rs`）：用 `Mutex` 串行化所有 env 访问，并在测试模块顶部 `#![allow(unsafe_code)]` 加注释说明：

```rust
#![allow(unsafe_code)] // 测试中 set_var/remove_var 在 Rust 2024 标记为 unsafe
// SAFETY: 持有 ENV_LOCK 保证串行，无并发 set_var 风险。
unsafe { env::set_var(key, value) };
```

`EnvGuard` RAII 模式确保测试结束恢复原值（`Drop` 中 `set_var`/`remove_var` 还原）。

#### 预防措施

- 任何新代码避免在运行时修改环境变量——凭证走 OS keyring（C-04），配置走 `RuntimeConfig` 分层加载；
- 必须修改时，确保在「单线程阶段」调用（如 `main` 入口、子进程 fork 后 exec 前）；
- 测试中统一用 `EnvGuard`（见 `crates/minicoding-core/src/paths.rs` 测试模块），不要裸调 `set_var`；
- `clippy::pedantic` 开启 `undocumented_unsafe_blocks` lint，强制每个 `unsafe` 块带 `// SAFETY:` 注释（见 `AGENTS.md` §2.6）。

---

### 2.5 nix v0.28.0 被未来 Rust 版本拒绝

#### 问题描述

`cargo update` 或升级 Rust nightly 后，`cargo check` 报 `nix` crate 中 `libc` API 调用被 deprecated/移除：

```text
error: use of deprecated function `libc::symbol`: replaced by `libc::dlsym`
  --> ~/.cargo/registry/src/.../nix-0.28.0/src/unistd.rs:...
```

或未来 Rust 版本编译期直接拒绝 `nix 0.28.0` 的某些不安全模式。

#### 原因分析

`nix` 是 Unix 系统调用封装 crate（项目通过 `landlock`/`sandbox-run`/`indicatif` 等传递引入）。`nix 0.28.0` 是较旧版本，使用了已被后续 `libc` 标记 deprecated 的 API，且部分用法在未来 Rust edition 中可能不再编译。

项目当前 `Cargo.lock` 已升级到 `nix 0.29.0` 与 `nix 0.31.3`（双版本共存，见 `Cargo.lock` 第 3446/3459 行），`nix 0.28.0` 已不在依赖树中。但若 PR 引入新的传递依赖拉回旧 `nix`，问题会复现。

#### 解决方案

检查当前依赖树中的 `nix` 版本：

```bash
cargo tree -i nix | head -n 30
```

若有 `nix 0.28.0` 残留，升级到最新补丁版本：

```bash
cargo update -p nix
```

若多个 major 版本共存（如 `0.29.0` 与 `0.31.3`），评估是否能统一到单一版本（`cargo tree -d nix` 查看重复来源），通过升级上游 crate 消除重复。

`deny.toml` 的 `[bans] multiple-versions = "warn"` 会在 CI 报警告但不阻断，必要时在 `[bans] skip` 中显式 skip 不可消除的重复版本（需附注释说明）。

#### 预防措施

- 定期 `cargo update` 升级补丁版本（CI 的 `Swatinem/rust-cache` 缓存依赖编译产物，升级成本低）；
- `cargo deny check bans` 在 CI 监控重复依赖；
- 引入新依赖前用 `cargo tree -d` 检查是否会引入旧 `nix`；
- `nix` 是 Unix-only 传递依赖，Windows 不编译，无需 Windows 特殊处理。

---

### 2.6 cargo audit --deny vulnerabilities 无效选项

#### 问题描述

为了让 CI 在漏洞时严格失败，尝试在 `ci.yml` 加：

```yaml
- name: cargo audit
  run: cargo audit --deny vulnerabilities
```

但 CI 报错：

```text
error: unexpected argument '--deny' found
  tip: a similar argument exists: '--ignore'

Usage: cargo audit [OPTIONS]
```

或本地 `cargo audit --deny vulnerabilities` 同样失败。

#### 原因分析

`cargo-audit` 的 CLI 设计与 `cargo-deny` 不同：`cargo audit` **默认**就让 vulnerabilities 失败（exit code 非 0），`unmaintained`/`yanked`/`notices` 仅警告。`--deny` 不是 `cargo audit` 的子命令参数（`cargo-deny` 才有 `--deny`）。

项目历史上曾尝试用 `--deny warnings` 让 unmaintained 也失败，但因 `number_prefix 0.4.0`（`indicatif` 传递依赖，RUSTSEC-2025-0119 unmaintained）会阻断 CI 而放弃——见 §3.4。

#### 解决方案

直接用 `cargo audit`（不加任何 `--deny`）：

```yaml
# .github/workflows/ci.yml
- name: cargo audit
  # `cargo audit` 默认：vulnerabilities 失败、unmaintained/yanked/notices 仅警告。
  # 不使用 `--deny warnings`：number_prefix 0.4.0 是 indicatif 的传递依赖
  # （RUSTSEC-2025-0119 unmaintained），CI 不应因 unmaintained 警告阻断。
  # `--deny vulnerabilities` 是无效参数（vulnerabilities 默认就 deny）。
  run: cargo audit
```

`scripts/git-hooks/pre-commit` 的 pre-push 阶段同样不加 `--deny`（见第 54-55 行注释）。

若需忽略特定 advisory（如已评估风险的 unmaintained），在 `audit.toml`（若有）中：

```toml
[advisories]
ignore = ["RUSTSEC-2025-0119"]  # 需附 reason 注释
```

或用 `cargo audit --ignore RUSTSEC-2025-0119` 临时忽略。

#### 预防措施

- 记住 `cargo audit` 与 `cargo deny check advisories` 是互补关系：前者查 RUSTSEC 漏洞库，后者额外查许可证/bans/sources；
- CI 的 `audit` job 与 `deny` job 分离，各司其职（见 `ci.yml`）；
- 不要复制 `cargo-deny` 的 `--deny` 语法到 `cargo audit`；
- 新成员 PR 若 CI `audit` job 失败，先用 `cargo audit` 本地复现，按 RUSTSEC ID 评估是漏洞还是 unmaintained。

---

### 2.7 ts-rs 生成的 TypeScript 文件有 trailing whitespace

#### 问题描述

提交 PR 时 pre-commit hook 报错：

```text
[pre-commit] trailing whitespace / EOF newline / secrets ...
  trailing whitespace: crates/minicoding-web/src/api/generated/Task.ts
  trailing whitespace: crates/minicoding-web/src/api/generated/Command.ts
```

但开发者并未手动编辑这些文件——它们是 `cargo test -p minicoding-core --features ts` 时 `ts-rs` 自动生成的。

#### 原因分析

`minicoding-protocol` 与 `minicoding-core` 的 DTO 通过 `ts-rs` crate（见 `Cargo.toml` 第 134 行）自动生成 TypeScript 类型到 `crates/minicoding-web/src/api/generated/`（`AGENTS.md` §8.4 DTO 自动生成）。`ts-rs` 的代码生成器在某些类型（如带 doc comment 的 enum/struct）输出末尾会带 trailing whitespace，这是库的输出特性，非开发者可控。

pre-commit hook 默认检查所有暂存文件的 trailing whitespace（见 `scripts/git-hooks/pre-commit` 第 19-40 行），生成的文件会误报。

#### 解决方案

pre-commit hook 已在 `EXCLUDE_PATTERN` 中跳过生成目录（见 `scripts/git-hooks/pre-commit` 第 22-28 行）：

```bash
# 排除 ts-rs 自动生成的文件（trailing whitespace 是库输出特性，见 gen-types 脚本后处理）
EXCLUDE_PATTERN='crates/minicoding-web/src/api/generated/'
while IFS= read -r f; do
  if [ -f "$f" ] && [ -s "$f" ]; then
    # 跳过自动生成的文件
    case "$f" in
      *"$EXCLUDE_PATTERN"*) continue ;;
    esac
    # ... trailing whitespace / EOF newline 检查
```

若仍报错（如 hook 版本未更新），重新安装 hook：

```bash
cp scripts/git-hooks/pre-commit .git/hooks/pre-commit
chmod +x .git/hooks/pre-commit
```

若需手动清理生成文件的 trailing whitespace（如本地 lint 强迫症），可在 `gen-types` 脚本后处理阶段加 `sed -i 's/[[:space:]]*$//'`，但**不要手动编辑**生成文件（文件头已标注 `// AUTO-GENERATED, DO NOT EDIT`）。

#### 预防措施

- 后端 DTO 变更后，跑 `cargo test -p minicoding-core --features ts` 重新生成，再 `git add crates/minicoding-web/src/api/generated/`；
- CI 应有「生成产物与 Rust 源一致」校验（`git diff --exit-code` after `npm run gen-types`），防止漏提交；
- pre-commit hook 的 `EXCLUDE_PATTERN` 只针对生成目录，**手写的前端代码**仍受 trailing whitespace 检查约束；
- 不要在 `generated/` 目录下手写任何文件——下次 `gen-types` 会覆盖。

---

### 2.8 serde_json::Value 缺少 TS trait

#### 问题描述

启用 `ts` feature 编译 `minicoding-core` 时报错：

```text
error[E0277]: the trait bound `serde_json::Value: TS` is not satisfied
  --> crates/minicoding-core/src/model/tool.rs:XX:Y
   |
   | #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
   |                          ^^^^^^^^^^^^^^^^ the trait `TS` is not implemented for `serde_json::Value`
```

或 DTO 中含 `serde_json::Value` 字段（如 `ToolResult::ok_json`、Hook 的 `extras`/`modify_input`）时编译失败。

#### 原因分析

`ts-rs` 默认为基本类型（`String`/`Vec`/`Option`/自定义 struct/enum）实现 `TS` trait，但 `serde_json::Value` 是通用 JSON 值类型，`ts-rs` 不默认为其实现 `TS`——因为 JSON 值可能对应 TS 的 `any`/`unknown`/`Record<string, unknown>` 等多种类型，需要用户显式选择。

项目 DTO 中 `ToolContent::Json(serde_json::Value)`、`HookInput.extras`、`HookOutput.modify_input` 等字段含 `serde_json::Value`，开启 `ts` feature 时 `derive(ts_rs::TS)` 会要求 `serde_json::Value: TS`。

#### 解决方案

`Cargo.toml` 已为 `ts-rs` 启用 `serde-json-impl` feature（见第 133-134 行）：

```toml
# `serde-json-impl` 提供 `serde_json::Value` → `any` 的 TS 实现（DTO 含 JSON 字段）
ts-rs = { version = "10", features = ["no-serde-warnings", "serde-json-impl"] }
```

`serde-json-impl` 让 `ts-rs` 把 `serde_json::Value` 映射为 TS 的 `any`（运行时再由 Zod schema 校验，见 `AGENTS.md` §8.4）。

若仍报错，确认：

1. `Cargo.toml` workspace dependencies 的 `ts-rs` 含 `serde-json-impl` feature；
2. 各 crate 的 `Cargo.toml` 用 `ts-rs = { workspace = true }` 继承，不要覆盖 feature；
3. `crates/minicoding-core/Cargo.toml` 的 `ts` feature 正确开启 `dep:ts-rs`：

```toml
[features]
ts = ["dep:ts-rs"]
```

对于需要更精确类型的字段（如某 JSON 字段实际是固定 schema），用 `#[cfg_attr(feature = "ts", ts(type = "{ secs: number; nanos: number }"))]` 显式标注（见 `crates/minicoding-core/src/model/tool.rs` 第 91 行 `ToolResultMeta.elapsed`）。

#### 预防措施

- 新增 DTO 含 `serde_json::Value` 字段时，确认 `ts-rs` 已开 `serde-json-impl`；
- 优先用具体类型（如自定义 struct）而非 `serde_json::Value`，仅在 LLM/外部 JSON 不可预测时用 `Value`；
- `ts-rs` 升级时检查 `serde-json-impl` feature 是否仍存在（`ts-rs` 10.x 起改为 feature flag）；
- 生成的前端类型在 Zod parse 前视为 `any`，**必须**经 Zod 校验后才进入业务层（`AGENTS.md` §8.4 运行时校验）。

---

### 2.9 TaskStatus 序列化不匹配（in_progress vs inprogress）

#### 问题描述

调用 `task.list` 工具传 `{"status": "in_progress"}` 时，LLM 收到错误「unknown variant `in_progress`, expected one of `pending`, `inprogress`, `completed`, `cancelled`」。而文档 `docs/design.md` §18 与 `docs/dev-plan.md` 中描述任务状态为 `in_progress`（带下划线）。

前端 `crates/minicoding-web/src/api/generated/TaskStatus.ts` 显示：

```typescript
export type TaskStatus = "pending" | "inprogress" | "completed" | "cancelled";
```

`task.list` 的 input_schema 也声明 `enum: ["pending", "inprogress", "completed", "cancelled"]`（见 `crates/minicoding-tools/src/task/list.rs` 第 29 行）。

但 `TaskStatus::as_str()` 返回 `"in_progress"`（带下划线，见 `crates/minicoding-core/src/model/task.rs` 第 32 行）。

#### 原因分析

`TaskStatus` enum 使用 `#[serde(rename_all = "lowercase")]`（见 `crates/minicoding-core/src/model/task.rs` 第 18 行），serde 把 `InProgress` 直接转小写为 `"inprogress"`（**无下划线**）。而 `as_str()` 方法是手写的，返回 `"in_progress"`（带下划线），与 serde 序列化结果不一致。

文档 `docs/design.md` §18 与 `docs/dev-plan.md` 沿用了 `as_str()` 的 `"in_progress"` 写法，导致「文档说 `in_progress`，实际序列化是 `inprogress`」的漂移。

LLM 看 input_schema 与 TS 类型用的是 serde 的 `"inprogress"`，但看 `as_str()` 错误消息或文档时认为是 `"in_progress"`，传参就会失败。

#### 解决方案

**方案 A（推荐）：统一 serde 序列化为 `in_progress`**

修改 `crates/minicoding-core/src/model/task.rs`，把 `rename_all = "lowercase"` 换成显式 `rename`：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "../../minicoding-web/src/api/generated/")
)]
#[serde(rename_all = "snake_case")]  // 或显式 #[serde(rename = "in_progress")] on InProgress
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}
```

`snake_case` 会把 `InProgress` 转为 `"in_progress"`，与 `as_str()` 一致。同步更新 `task.list` 的 input_schema enum 为 `["pending", "in_progress", "completed", "cancelled"]`，重新生成 TS 类型。

**方案 B：统一 `as_str()` 为 `inprogress`**

若已有外部消费者依赖 `"inprogress"`，则修改 `as_str()` 返回 `"inprogress"`，并更新文档。但这与 JSON 惯例（snake_case）不符，不推荐。

**生成 TS 与更新 schema 后**：

```bash
cargo test -p minicoding-core --features ts  # 重新生成 TaskStatus.ts
git add crates/minicoding-web/src/api/generated/TaskStatus.ts
```

#### 预防措施

- enum 用 `serde` 重命名时，`as_str()`（或类似手写方法）必须与 serde 输出**完全一致**，否则错误消息与实际序列化漂移；
- 优先用 `#[serde(rename_all = "snake_case")]` 而非 `lowercase`——snake_case 是 JSON 惯例，且与 `as_str()` 自然对齐；
- 文档中的枚举字符串值以 serde 序列化结果为准，`as_str()` 仅用于错误消息时也要对齐；
- CI 加「DTO 序列化快照测试」（`insta` assert snapshot of `serde_json::to_string(&TaskStatus::InProgress)`），序列化漂移时 CI 失败；
- `task.list` 的 input_schema enum 必须与 serde 反序列化接受的值一致，否则 LLM 传参失败。

---

## 3. CI/CD 问题

### 3.1 coverage 排除前端 crate

#### 问题描述

CI 的 `coverage` job 报覆盖率不达标（< 80%），但失败原因集中在 `minicoding-tui`/`minicoding-cli`/`minicoding-server`/`minicoding-desktop`——这些 crate 单测覆盖率天然低（需 TTY/真实端口/GUI 运行时），拖累整体数字。

#### 原因分析

`AGENTS.md` §2.8 覆盖率目标 ≥80% 是针对**库 crate**（有纯逻辑可单测），而前端层 crate 的覆盖应由集成测试保证：

- `minicoding-tui`：终端渲染，需 TTY 仿真，单测难覆盖渲染逻辑；
- `minicoding-cli`：入口 bin，集成测试覆盖（`assert_cmd`）；
- `minicoding-server`：HTTP/SSE/ACP/LSP，需真实端口+客户端，单测覆盖率低；
- `minicoding-desktop`：Tauri 桌面壳，需 GUI 运行时。

把前端层纳入 `--fail-under-lines 80` 会让数字虚低，且鼓励写无意义的「为覆盖率而测」测试。

#### 解决方案

CI 的 `coverage` job 已 `--exclude` 这四个 crate（见 `.github/workflows/ci.yml` 第 91 行）：

```yaml
- name: cargo llvm-cov
  # 排除前端层（与 AGENTS.md §2.8 库 crate 覆盖率目标 ≥80% 对齐）：
  # - minicoding-tui：终端渲染，需 TTY 仿真
  # - minicoding-cli：入口 bin，集成测试覆盖
  # - minicoding-server：HTTP/SSE/ACP/LSP 前端层，集成测试覆盖
  # - minicoding-desktop：Tauri 桌面壳，需 GUI 运行时，集成测试覆盖
  run: cargo llvm-cov --workspace --exclude minicoding-desktop --all-features \
    --exclude minicoding-tui --exclude minicoding-cli --exclude minicoding-server \
    --fail-under-lines 80
```

pre-commit hook 的 pre-push 阶段同步排除（见 `scripts/git-hooks/pre-commit` 第 63-65 行）。

#### 预防措施

- 新增前端层 crate（如未来加 `minicoding-mobile`）时，同步加入 `--exclude` 列表；
- 库 crate（core/context/policy/memory/hooks/journal/sandbox/mcp/storage/providers/tools/protocol/extension-sdk/sdk）必须保持 ≥80%；
- 不要为了凑覆盖率在 `tui`/`cli`/`server` 写「调用一次渲染函数断言不 panic」的无效测试——改用集成测试覆盖关键路径；
- 定期 review `cargo llvm-cov --workspace --exclude ... --html` 报告，关注库 crate 中未覆盖的分支。

---

### 3.2 desktop CI 需要系统库

#### 问题描述

PR 改动 `minicoding-desktop` 后，CI 的 `desktop` job 失败：

```text
Run cargo build -p minicoding-desktop --features desktop
  error: failed to run custom build command for `glib-sys`
  = note: pkg-config exited with status code 1
```

而其他 job（`clippy`/`test`/`coverage`）正常通过。

#### 原因分析

CI 的 `clippy`/`test`/`coverage`/`cross-platform` job 都 `--exclude minicoding-desktop`，不编译桌面 crate，所以不触发 Tauri 系统依赖缺失。`desktop` job 单独安装系统库后编译（见 `.github/workflows/ci.yml` 第 161-175 行），若安装步骤失败或包名变更，`cargo build` 会因 `pkg-config` 找不到 `glib-2.0` 等失败。

常见触发场景：

1. Ubuntu LTS 版本升级导致 `libwebkit2gtk-4.1-dev` 包名变化（如 4.0 → 4.1）；
2. `apt-get update` 未执行导致包索引过期；
3. runner 镜像变更缺少前置依赖。

#### 解决方案

CI 的 `desktop` job 已固化安装步骤（见 `ci.yml`）：

```yaml
- name: Install Tauri system dependencies
  run: |
    sudo apt-get update
    sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libglib2.0-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev
- name: cargo build desktop
  run: cargo build -p minicoding-desktop --features desktop
- name: cargo clippy desktop
  run: cargo clippy -p minicoding-desktop --features desktop -- -D warnings
```

若失败，检查：

1. `apt-get update` 是否成功（网络/镜像问题）；
2. 包名是否仍存在（`apt-cache show libwebkit2gtk-4.1-dev`）；
3. runner 镜像版本（`ubuntu-latest` 可能切换到新 LTS）。

本地复现见 §2.2。

#### 预防措施

- `desktop` job 用 `ubuntu-latest` 而非固定版本，跟随官方 runner 升级；
- `apt-get update` 必须在 `install` 前执行；
- 改动 `minicoding-desktop` 的 `Cargo.toml` 新增 Tauri 相关依赖时，本地先验证 `ci.yml` 的安装步骤够用；
- macOS/Windows 的 `desktop` 编译走 `cross-platform` job（`desktop` feature 在非 Linux 也需 webview，但 macOS/Windows 用系统自带 webview，无需额外系统包）。

---

### 3.3 cargo deny 许可证白名单需包含 AGPL-3.0-only/BSL-1.0

#### 问题描述

CI 的 `deny` job 失败：

```text
error: license = "AGPL-3.0-only" rejected
  --> crates/minicoding-core/Cargo.toml
```

或 `cargo deny check licenses` 报 `BSL-1.0`/`OpenSSL`/`Unicode-DFS-2016` 等许可证未在白名单。

#### 原因分析

`AGENTS.md` §2.7 规定许可证限 MIT/Apache-2.0/BSD/ISC，但项目实际依赖树包含以下额外许可证：

| 许可证 | 来源 | 性质 |
|--------|------|------|
| `AGPL-3.0-only` | 项目自身 crate（`Cargo.toml` 第 28 行 `license = "AGPL-3.0-only"`） | workspace 成员，非外部依赖 |
| `BSL-1.0` | `error-code` 等 transitive crate | Boost Software License，类 MIT 宽松 |
| `OpenSSL` | `ring`（rustls/reqwest 的 crypto backend） | 含 OpenSSL 衍生代码 |
| `Unicode-DFS-2016` / `Unicode-3.0` | `unicode-ident`（proc-macro2 传递） | Unicode 词表许可 |
| `Zlib` | `miniz_oxide`（flate2 传递） | 宽松许可 |
| `CC0-1.0` | `adler`（miniz_oxide 传递） | 公共领域 |
| `Unlicense` | 部分 transitive crate | 宽松 |
| `CDLA-Permissive-2.0` | 部分 transitive crate | 数据许可 |

`cargo deny` 默认严格——任何不在白名单的许可证都 deny。

#### 解决方案

`deny.toml` 的 `[licenses] allow` 已包含上述许可证（见 `deny.toml` 第 36-52 行）：

```toml
allow = [
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "AGPL-3.0-only",      # 项目自身 crate
    "BSL-1.0",            # error-code 等 transitive
    "OpenSSL",            # ring (rustls backend)
    "Unicode-DFS-2016",   # unicode-ident
    "Unicode-3.0",
    "Zlib",               # miniz_oxide
    "CC0-1.0",            # adler
    "Unlicense",
    "CDLA-Permissive-2.0",
]
confidence-threshold = 0.8
```

每条额外许可证都带注释说明来源（见 `deny.toml` 第 26-34 行）。

新增依赖引入新许可证时：

1. `cargo deny check licenses` 本地复现；
2. 评估许可证兼容性（参考 SPDX 分类）；
3. 若是宽松许可证（类 MIT），加入 `allow` 列表并附注释；
4. 若是 copyleft（GPL/LGPL/AGPL 外部依赖）或未知许可证，**拒绝引入**，寻找替代 crate。

#### 预防措施

- 新增依赖前跑 `cargo deny check licenses` 验证；
- `deny.toml` 的 `allow` 列表每条带注释说明「为何允许」；
- `confidence-threshold = 0.8` 平衡误报与漏报；
- `minicoding-desktop` 的 Tauri 依赖通过 `[graph] exclude = ["minicoding-desktop"]` 排除许可证检查（Tauri 传递依赖许可证复杂，desktop 为可选 feature）；
- 项目自身用 `AGPL-3.0-only`（见 `Cargo.toml`），与 `AGENTS.md` §2.7「许可证限 MIT/Apache-2.0/BSD/ISC」的约束是**外部依赖治理**，项目自身许可证不受此约束。

---

### 3.4 number_prefix 0.4.0 未维护（RUSTSEC-2025-0119）

#### 问题描述

`cargo audit` 输出警告：

```text
warning: unmaintained crate
  ID: RUSTSEC-2025-0119
  Package: number_prefix 0.4.0
  Title: number_prefix is unmaintained
  Date: 2025-...
  URL: https://rustsec.org/advisories/RUSTSEC-2025-0119
```

CI 的 `audit` job 不因警告失败（默认只 deny vulnerabilities），但 `cargo audit --deny warnings` 会失败。

#### 原因分析

`number_prefix 0.4.0` 是 `indicatif 0.17.11`（进度条 crate，见 `Cargo.toml` 第 78 行）的传递依赖（见 `Cargo.lock` 第 2400 行），用于数字前缀格式化（如 `1.5K`/`2.3M`）。`number_prefix` 作者已停止维护，RUSTSEC-2025-0119 标记为 unmaintained。

**注意**：`deny.toml` 第 16-17 行注释声称「number_prefix 不再出现在依赖树中（indicatif 已切换替代或移除该传递依赖）」，但 `Cargo.lock` 第 2394-2404 行显示 `indicatif 0.17.11` 仍依赖 `number_prefix 0.4.0`——**文档与实现存在漂移**，需更新注释或升级 `indicatif`。

#### 解决方案

**短期**：CI 不用 `--deny warnings`，让 unmaintained 仅警告（见 `ci.yml` 第 106-110 行注释）：

```yaml
- name: cargo audit
  # 不使用 `--deny warnings`：number_prefix 0.4.0 是 indicatif 的传递依赖
  # （RUSTSEC-2025-0119 unmaintained），CI 不应因 unmaintained 警告阻断。
  run: cargo audit
```

**中期**：升级 `indicatif` 到已移除 `number_prefix` 传递依赖的版本（若上游已修复），或评估替代 crate：

```bash
cargo update -p indicatif
cargo tree -i number_prefix  # 确认是否仍存在
```

若 `indicatif` 升级后仍传递 `number_prefix`，在 `audit.toml` 显式 ignore（需附 reason）：

```toml
[advisories]
ignore = ["RUSTSEC-2025-0119"]  # number_prefix unmaintained，indicatif 传递依赖，无替代
```

**长期**：`indicatif` 仅用于 CLI 进度条，评估是否能用更轻量的 `anstream` + 自实现进度替代，消除传递依赖。

**文档漂移修复**：更新 `deny.toml` 第 16-17 行注释，反映 `number_prefix` 仍存在的事实，或升级后删除该注释。

#### 预防措施

- `cargo audit` 与 `cargo deny check advisories` 在 CI 双重监控；
- `deny.toml` 的 `[advisories] unmaintained = "workspace"` 仅检查 workspace 成员，不检查传递依赖的 unmaintained（避免 `number_prefix` 这类不可控传递依赖阻断）；
- 定期 `cargo update` 升级补丁版本，跟进上游修复；
- `deny.toml` 注释必须与 `Cargo.lock` 实际依赖树一致——改依赖时同步改注释（`AGENTS.md` §4.1 改代码必改文档）；
- 引入新依赖前 `cargo tree -d` 与 `cargo audit` 检查传递依赖健康度。

---

### 3.5 pre-commit hook 与生成文件冲突

#### 问题描述

提交包含 `crates/minicoding-web/src/api/generated/` 下文件的 PR 时，pre-commit hook 报 trailing whitespace 或 EOF newline 错误（见 §2.7），或 `cargo fmt --check` 报「生成文件不符合 rustfmt」——但这些文件是 ts-rs 自动生成的 `.ts` 文件，不应被 rustfmt 检查。

另一种场景：`cargo clippy` 在 pre-commit 阶段编译 `minicoding-desktop` 失败，因本地未装 Tauri 系统库（见 §2.2）。

#### 原因分析

pre-commit hook 默认对所有暂存文件跑 `cargo fmt --check`/`clippy`/`deny`/trailing whitespace 检查（见 `scripts/git-hooks/pre-commit`）。生成文件（`.ts`/`.snap`/`*.generated.rs`）与平台特定 crate（`minicoding-desktop`）需要显式排除，否则误报。

#### 解决方案

pre-commit hook 已做两类排除：

1. **trailing whitespace 检查排除生成目录**（见 `scripts/git-hooks/pre-commit` 第 22-28 行）：

```bash
EXCLUDE_PATTERN='crates/minicoding-web/src/api/generated/'
case "$f" in
  *"$EXCLUDE_PATTERN"*) continue ;;
esac
```

2. **cargo clippy/test 排除 minicoding-desktop**（见第 13-14、58-59 行）：

```bash
cargo clippy --workspace --exclude minicoding-desktop --all-targets --all-features -- -D warnings
cargo test --workspace --exclude minicoding-desktop --all-features
```

若仍报错：

- **生成文件未更新**：后端 DTO 变更后忘了重新生成，`git status` 显示生成文件被改但内容是旧的——跑 `cargo test -p minicoding-core --features ts` 重新生成后 `git add`；
- **rustfmt 误检 `.ts` 文件**：`cargo fmt` 只检查 `.rs` 文件，`.ts` 不会被检查，若报错说明 hook 逻辑有误，检查 `pre-commit` 脚本；
- **本地未装 Tauri 系统库**：见 §2.2，或日常开发用 `--exclude minicoding-desktop`（hook 已默认排除）。

#### 预防措施

- pre-commit hook 的排除规则与 `ci.yml` 保持一致（`--exclude minicoding-desktop`、`EXCLUDE_PATTERN` 生成目录）；
- 新增生成文件目录时（如未来加 `specta` 生成），同步加入 `EXCLUDE_PATTERN`；
- 后端 DTO 变更的 PR 必须包含生成文件的更新（CI 应有 `git diff --exit-code` 校验）；
- 不要在 `pre-commit` 跑 `cargo audit`（慢，且 unmaintained 警告会阻断提交）——`audit` 放 pre-push 阶段或 CI（见 `scripts/git-hooks/pre-commit` 第 51-55 行）。

---

## 4. 运行时问题

### 4.1 API Key 未配置

#### 问题描述

运行 `minicoding "..."` 后立即报错：

```text
Error: LlmProvider 调用失败: missing API key

Caused by:
    OPENAI_API_KEY environment variable not set
```

或 `minicoding --session` 进入 REPL 后第一条消息返回 401 Unauthorized。

#### 原因分析

凭证只从环境变量或 OS keyring 读取，**绝不**写入配置文件明文（`AGENTS.md` §5.3、`docs/security.md` §6、C-04）。未设置时 provider 构造阶段直接失败，不进入 Agent 循环。

#### 解决方案

**方式一：环境变量（最简）**

```bash
# OpenAI 兼容（M1 默认 provider）
export OPENAI_API_KEY="sk-..."

# Anthropic（M6 交付）
export ANTHROPIC_API_KEY="sk-ant-..."

# 本地 Ollama（无需 key，确保 ollama serve 在 127.0.0.1:11434）
```

Windows PowerShell：

```powershell
$env:OPENAI_API_KEY = "sk-..."
```

写入 shell 配置（`~/.zshrc`/`~/.bashrc`）持久化，但注意不要把含 key 的配置提交到 dotfiles 仓库。

**方式二：OS keyring（推荐，交互场景）**

```bash
minicoding auth login --provider anthropic
# 输入密钥（不回显）→ 写入 OS keyring（KEYRING_SERVICE = "minicoding"）
minicoding auth status
```

keyring 由 OS 加密存储（macOS Keychain / Windows Credential Manager / Linux Secret Service），比环境变量更安全。

**方式三：`env:VAR_NAME` 语法（config.toml 引用）**

```toml
# ~/.minicoding/config.toml
[providers.anthropic]
api_key = "env:ANTHROPIC_API_KEY"  # 引用环境变量，不写明文
```

支持 `env:VAR_NAME:-fallback` 回退语法（见 `docs/tech-stack.md` §12）。

#### 预防措施

- 首次运行前必读 `docs/getting-started.md` §1.3 配置 API Key；
- CI 测试用 mock 凭证 `OPENAI_API_KEY=sk-test-ci-mock-not-real`（见 `ci.yml` 第 69 行），不连真实服务（`AGENTS.md` §5.4）；
- 不要把 API key 写入 `config.toml` 明文、commit message、PR 描述、日志（C-04）；
- 日志中密钥脱敏（前 4 字符 + `***`，见 `policy::redact`）；
- `minicoding auth status` 可随时检查凭证状态。

---

### 4.2 非 TTY 下副作用工具被拒

#### 问题描述

在 CI / 容器 / 管道中运行 `minicoding exec "..."` 时，所有副作用工具（`fs.write`/`shell.run`/`web.fetch`）都被拒：

```text
[permission] fs.write src/main.rs → DENY (non-interactive, default deny)
```

而交互式终端（TTY）下会弹窗询问。

#### 原因分析

非 TTY 环境无法弹窗交互，`NonInteractivePrompter` 默认策略是 `deny`（`docs/security.md` §2.1、`docs/getting-started.md` §1.5 表格）。这是安全默认——避免在 CI 中意外执行写操作。

#### 解决方案

**方式一：显式 `--allow` 预批准特定工具**

```bash
minicoding exec --allow "fs.write:src/**" --allow "shell.run:cargo *,git *" "重构 utils 模块"
```

`--allow` 展开为 specificity=2 的 L1 规则（见 `docs/getting-started.md` §3.5），仍受 L0 黑名单约束（C-02）。

**方式二：切换 `permission.non_tty_strategy`**

```toml
# ~/.minicoding/config.toml
[permission]
non_tty_strategy = "allow"  # 或 "ask"（需配合 --prompter callback）
```

`allow` 会批准所有非 L0 黑名单的请求（危险，仅在受信沙箱内用）。

**方式三：用预设**

```bash
# 只读（代码审计、日志诊断）
minicoding exec --sandbox read-only "审计 src/ 目录"

# 外部沙箱（CI/容器内批量任务，依赖容器隔离）
minicoding exec --sandbox external-sandbox "跑全套测试"

# 完全访问（需显式确认 + red 警告，仅受信环境）
minicoding exec --preset full-access "全自动部署"
```

**方式四：SDK CallbackPrompter（嵌入式）**

```rust
// 用 minicoding-sdk 时，闭包处理权限请求
client.ask(input, CallbackPrompter(|req| {
    println!("approve? {}", req.tool);
    Decision::Allow
})).await?;
```

#### 预防措施

- CI/容器中默认用 `--sandbox read-only` 或 `external-sandbox`，不裸跑 `full-access`；
- `--allow` 用最小权限原则，只批准必要的工具+路径/命令前缀；
- `non_tty_strategy = "allow"` 仅在受信沙箱（容器/VM）内启用；
- `DangerFullAccess` 启动时强制 red 警告 + 二次确认（C-22），非 TTY 下无法确认故无法启用。

---

### 4.3 Linux 内核 < 5.13 不支持 Landlock

#### 问题描述

`minicoding doctor --security` 输出：

```text
Sandbox Driver: NoopDriver (degraded)
Hardened: false
Warning: Landlock not available (kernel < 5.13), falling back to application-layer sandbox only.
```

或运行时日志：

```text
WARN sandbox: landlock not available, falling back to NoopDriver
```

#### 原因分析

`landlock` crate 依赖 Linux 5.13+ 的 Landlock LSM（Linux Security Module）。`minicoding-sandbox::detect_driver()` 在运行时调用 `sandbox_run::landlock_available()` 探测内核支持（见 `docs/tech-stack.md` §11、`docs/modules.md` §7.4）：

- 内核 5.13+：启用 Landlock + libseccomp，`is_hardened()` 返回 `true`；
- 内核 < 5.13：降级为 `NoopDriver`（来自 `minicoding-core`），打 `warn` 日志，仅应用层权限（`sandbox_path` + `PermissionPolicy`）生效。

这是设计内的 fail-open 降级，不阻塞编译与运行（见 `docs/getting-started.md` §1.5）。

#### 解决方案

**检测内核版本**：

```bash
uname -r  # 应 >= 5.13.0
```

**升级内核**（若可能）：

```bash
# Ubuntu HWE 内核
sudo apt install --install-recommends linux-generic-hwe-$(lsb_release -rs)

# 或手动升级，重启后生效
```

**不升级内核的缓解**：

1. 用容器/WSL2 做硬隔离：`docker run --rm -it ... minicoding exec ...`，依赖容器 namespace 隔离；
2. 显式选 `--preset external-sandbox`：声明依赖外部容器隔离（`docs/security.md` §8.1、§9.2），应用层权限仍生效；
3. 应用层权限（`sandbox_path` + `PermissionPolicy` + builtin 黑名单）作为唯一防线，配置严格的 `policy.toml`。

**验证沙箱状态**：

```bash
minicoding doctor --security
# 应输出 Hardened: true（升级内核后）
```

#### 预防措施

- 生产部署前用 `minicoding doctor --security` 验证沙箱硬化状态；
- 旧内核环境（如 CentOS 7）优先用容器隔离 + `external-sandbox` 预设；
- `NoopDriver` 降级时 `is_hardened()` 返回 `false`，`doctor` 如实报告（C-22）；
- 不要在旧内核上启用 `full-access`——应用层权限被绕过时无内核级兜底。

---

### 4.4 Windows 沙箱成熟度低

#### 问题描述

Windows 上 `minicoding doctor --security` 输出：

```text
Sandbox Driver: WindowsJobObjectDriver (limited)
Hardened: false
Warning: Windows sandbox maturity is low, recommend running in WSL2/container.
```

或 `fs.write` 越界写未在内核级阻止（仅应用层 `sandbox_path` 拦截）。

#### 原因分析

Windows 缺乏 macOS Seatbelt / Linux Landlock 这样成熟的内核级 MAC 框架（`docs/security.md` §12、`docs/getting-started.md` §1.5）。M4 初期策略：

- 应用层路径沙箱 + 用户提示「Windows 沙箱降级，建议在 WSL2/容器内运行」；
- M6+ 补齐受限令牌 + Job Object + DACL（`windows` crate）；
- `doctor --security` 如实报告 `is_hardened() = false` 并建议 WSL2。

Windows 上 `cargo build -p minicoding-sandbox` 仍可通过（`landlock`/`libseccomp` 通过 `[target.'cfg(target_os = "linux")'.dependencies]` 条件引入，非 Linux 不编译，见 `AGENTS.md` §3.5）。

#### 解决方案

**推荐：WSL2 内运行**

```powershell
wsl --install -d Ubuntu
# 进入 WSL2 后按 Linux 流程安装 libseccomp-dev、cargo build
```

WSL2 用真实 Linux 内核（5.13+），Landlock 可用。

**或：容器隔离**

```powershell
docker run --rm -it -v ${PWD}:/work minicoding-rs cargo build --workspace
```

**或：接受降级，配置严格应用层权限**

```toml
# ~/.minicoding/config.toml
[permission]
preset = "auto"  # WorkspaceWrite，应用层路径沙箱 + 黑名单
non_tty_strategy = "deny"
```

`policy.toml` 配置严格的 `[[deny]]` 规则覆盖危险路径与命令。

#### 预防措施

- Windows 生产部署优先 WSL2 或容器；
- `doctor --security` 输出 `Hardened: false` 时不要启用 `full-access`；
- M6+ 后 Windows 受限令牌 + Job Object 补齐，`is_hardened()` 才返回 `true`；
- 跨平台 CI matrix（`ci.yml` 的 `cross-platform` job）验证 Windows 编译，但沙箱硬化测试仅在 Linux 跑。

---

### 4.5 沙箱降级为 NoopDriver

#### 问题描述

`minicoding doctor --security` 输出 `Sandbox Driver: NoopDriver`，或运行时无任何沙箱限制（`fs.write` 越界、`shell.run` 执行任意命令均未被内核级阻止）。

#### 原因分析

`NoopDriver`（来自 `minicoding-core`，见 `AGENTS.md` §3.4）是 `SandboxDriver` 的兜底实现，在以下场景降级：

1. Linux 内核 < 5.13（无 Landlock，见 §4.3）；
2. Windows 早期版本（Job Object 未补齐，见 §4.4）；
3. `--preset external-sandbox`（显式声明依赖外部容器隔离）；
4. `--preset full-access`（显式关闭沙箱，C-22 警告）；
5. `sandbox` feature 未启用（编译期 `default-features = false`）。

降级时仅应用层权限（`sandbox_path` + `PermissionPolicy` + builtin 黑名单）生效，无内核级兜底。

#### 解决方案

**确认降级原因**：

```bash
minicoding doctor --security
# 输出 Sandbox Driver 类型与 Hardened 状态
```

**按原因修复**：

| 原因 | 修复 |
|------|------|
| 内核 < 5.13 | 升级内核（见 §4.3）或用容器 |
| Windows | 用 WSL2/容器（见 §4.4）或等 M6+ |
| `external-sandbox` 预设 | 确认确实在容器/VM 内运行，否则改 `auto` 预设 |
| `full-access` 预设 | 改 `auto` 预设（`full-access` 仅受信沙箱内用） |
| `sandbox` feature 未启用 | `Cargo.toml` 开启 `default = ["sandbox"]` |

**验证恢复**：

```bash
minicoding doctor --security
# 应输出 Sandbox Driver: LandlockDriver (Linux) / SeatbeltDriver (macOS) / WindowsJobObjectDriver (Windows M6+)
# Hardened: true
```

#### 预防措施

- 生产部署前 `doctor --security` 必查；
- `NoopDriver` 降级时不要跑 `full-access`——应用层被绕过无兜底；
- `external-sandbox` 仅在确实有容器隔离时用，否则是裸奔；
- `sandbox` feature 默认启用（`default = ["memory", "sandbox"]`，见 `docs/modules.md` §0.4），不要为「减小二进制」关闭；
- `doctor --security` 如实报告降级状态（C-30 沙箱拒绝熔断不可被 LLM 绕过）。

---

## 5. 测试与覆盖率问题

### 5.1 覆盖率不达标（68% → 82.9%+ 历程）

#### 问题描述

某次 PR 的 CI `coverage` job 失败：

```text
FAIL: line coverage 76.2% < 80.0% threshold
```

或历史遗留的低覆盖率（如 68%）拖累整体数字。

#### 原因分析

覆盖率不达标的常见原因：

1. **未覆盖的分支**：错误处理路径（`Result::Err` 分支）、`cfg` 条件编译分支、边界条件未测；
2. **前端层混入统计**：`tui`/`cli`/`server`/`desktop` 单测覆盖率天然低（见 §3.1）；
3. **集成测试未纳入**：`tests/` 目录的集成测试覆盖的代码未计入 `llvm-cov` 统计；
4. **dead code**：未使用的函数/模块拉低分母命中率；
5. **测试 mock 不完整**：mock provider/tool 未触发某些路径。

#### 解决方案

**生成覆盖率报告定位未覆盖代码**：

```bash
cargo llvm-cov --workspace --exclude minicoding-desktop --all-features \
  --exclude minicoding-tui --exclude minicoding-cli --exclude minicoding-server \
  --html --output-dir coverage/
# 打开 coverage/index.html 查看逐行覆盖
```

**针对性补测试**：

1. 库 crate（core/context/policy/memory/hooks/journal/sandbox/mcp/storage/providers/tools/protocol/extension-sdk/sdk）未覆盖分支补单测；
2. 错误处理路径用 mock 触发 `Err`（如 `wiremock` 返回 500、`tempfile` 模拟权限错误）；
3. 集成测试放 `tests/` 目录，`llvm-cov` 自动纳入（`AGENTS.md` §2.8）；
4. dead code 用 `cargo dead` 或 `cargo udeps`（nightly）检测并删除。

**历史覆盖率提升路径（68% → 82.9%+）**：

- M0-M2 阶段覆盖核心 trait 与数据模型，达 68%；
- M3-M4 补 storage/sandbox/mcp 集成测试，达 75%；
- M5-M6 补 hooks/providers/tools 边界条件，达 80%；
- M7+ 持续补 TUI/CLI 集成测试（不计入统计但提升实际质量），库 crate 稳定在 82.9%+。

#### 预防措施

- 每个 PR 自查新增代码的覆盖率（`cargo llvm-cov --html` 局部查看）；
- CI `coverage` job `--fail-under-lines 80` 把关，不达标阻断合并（`ci.yml` 第 91 行）；
- 库 crate 目标 ≥80%，前端层 crate 由集成测试覆盖（见 §3.1）；
- 不写「为覆盖率而测」的无效测试（`AGENTS.md` §7.5 不创建测试代码除非要求）；
- 属性测试（`proptest`）覆盖不变量，比单测更能发现边界（如 `sandbox_path` 不变量、`Message` JSON roundtrip）。

---

### 5.2 wiremock set_body_string 强制 text/plain

#### 问题描述

用 `wiremock` mock HTTP 服务时，`set_body_string` 设置的响应 `Content-Type` 始终是 `text/plain`，即使手动 `insert_header("content-type", "text/event-stream")` 也被覆盖：

```rust
ResponseTemplate::new(200)
    .set_body_string(sse_body)
    .insert_header("content-type", "text/event-stream")
// 实际响应 Content-Type: text/plain，导致 SSE 解析失败
```

测试报错 `expected text/event-stream, got text/plain`。

#### 原因分析

`wiremock` 的 `set_body_string` 内部会设置 `Content-Type: text/plain` 作为默认，`insert_header` 在其后调用时若 key 已存在则**追加**而非替换（HTTP header 可重复），或 `set_body_string` 在 `insert_header` 之后再次覆盖。

项目 `web.fetch` 工具按 `Content-Type` 决定 HTML→Markdown 转换，`providers::openai` 按 `text/event-stream` 解析 SSE——mock 的 Content-Type 错误会导致测试逻辑路径错误。

#### 解决方案

用 `set_body_raw` 显式指定 `Content-Type`（见 `crates/minicoding-tools/src/web/fetch.rs` 第 219、255 行）：

```rust
// HTML 响应
ResponseTemplate::new(200)
    .set_body_raw(html.as_bytes().to_owned(), "text/html; charset=utf-8")

// SSE 流响应（providers 测试）
ResponseTemplate::new(200)
    .set_body_raw(sse_body.as_bytes().to_owned(), "text/event-stream")

// 纯文本
ResponseTemplate::new(200)
    .set_body_raw(body.as_bytes().to_owned(), "text/plain")
```

`set_body_raw` 接受 `&[u8]` + `content_type: &str`，直接设置 Content-Type 不被覆盖。

对于 `providers::openai` 的 SSE 测试，原代码用 `set_body_string` + `insert_header`（见 `crates/minicoding-providers/src/openai.rs` 第 787-788 行）能工作是因为 `insert_header` 在 `set_body_string` 之后调用且 wiremock 该版本下追加生效——但**不稳健**，建议统一改 `set_body_raw`。

#### 预防措施

- mock HTTP 响应统一用 `set_body_raw`，不用 `set_body_string`；
- `set_body_raw` 的 `content_type` 参数与生产响应的 Content-Type 一致；
- SSE 流 mock 用 `text/event-stream`，HTML 用 `text/html; charset=utf-8`，JSON 用 `application/json`；
- 测试断言响应 Content-Type 与生产一致，避免 mock 漂移；
- `wiremock` 升级时检查 `set_body_string`/`set_body_raw` 语义是否变化。

---

### 5.3 BoxFuture 未在作用域内

#### 问题描述

实现 `Tool` trait 时编译报错：

```text
error[E0433]: failed to resolve: use of unresolved type or module `BoxFuture`
  --> crates/minicoding-tools/src/my_tool.rs:42:65
   |
42 |     ) -> BoxFuture<'_, Result<ToolResult, ToolError>> {
   |          ^^^^^^^^^^ use of unresolved type or module `BoxFuture`
```

#### 原因分析

`Tool::execute` 返回 `BoxFuture<'_, Result<ToolResult, ToolError>>`（见 `crates/minicoding-core/src/tool.rs`），`BoxFuture` 是 `futures::future::BoxFuture` 的类型别名，需要 `use` 引入。

`AGENTS.md` §2.4 规定流式响应用 `BoxStream`/`impl Stream`，trait 方法返回 `BoxFuture` 保证 `dyn` 兼容（`async fn in trait` 的 trait 不是 object-safe，用 `BoxFuture` 显式 box）。

#### 解决方案

引入 `BoxFuture`：

```rust
use minicoding_core::provider::BoxFuture;  // 项目从 core re-export
```

或在实现中用全路径：

```rust
fn execute(
    &self,
    input: serde_json::Value,
    _ctx: &ToolContext,
) -> minicoding_core::provider::BoxFuture<'_, Result<ToolResult, ToolError>> {
    Box::pin(async move {
        // ...
        Ok(ToolResult::ok_text("..."))
    })
}
```

项目所有工具实现统一从 `minicoding_core::provider` 引入（见 `crates/minicoding-tools/src/task/create.rs` 第 5 行、`list.rs` 第 5 行、`spawn.rs` 第 22 行）。

#### 预防措施

- 实现工具时复制现有工具的 `use` 块（如 `crates/minicoding-tools/src/fs/read.rs`）；
- `BoxFuture` 从 `minicoding_core::provider` re-export，不直接 `use futures::future::BoxFuture`（保持依赖方向干净，core 是唯一 trait 来源）；
- `Box::pin(async move { ... })` 是标准模式，注意 `move` 捕获所有权；
- trait 需 `dyn` 兼容时用 `BoxFuture`，不需 `dyn` 时可用 `async fn`（但项目统一 `BoxFuture` 保持一致）。

---

### 5.4 ToolResult 缺少 text() 方法

#### 问题描述

测试中想提取 `ToolResult` 的文本内容，但 `ToolResult` 没有 `text()` 方法：

```rust
let result = tool.execute(input, &ctx).await.unwrap();
let text = result.text();  // 编译错误：no method named `text` found for `ToolResult`
```

不得不写 pattern match 辅助函数：

```rust
fn result_text(result: &ToolResult) -> &str {
    match &result.content {
        ToolContent::Text(t) => t,
        _ => "",
    }
}
```

#### 原因分析

`ToolResult` 结构体（见 `crates/minicoding-core/src/model/tool.rs` 第 104-108 行）：

```rust
pub struct ToolResult {
    pub content: ToolContent,
    pub is_error: bool,
    pub metadata: ToolResultMeta,
}
```

`impl ToolResult` 只提供构造方法（`ok_text`/`ok_json`/`err_text`，见第 110-133 行），未提供 `text()` getter。`ToolContent` 是 enum（`Text(String)`/`Json(Value)`/`Image`/`Mixed`），访问内容需 pattern match。

`ToolContent::text(s)` 是构造方法（第 77 行），不是 getter——命名冲突导致困惑。

测试代码不得不在每个测试模块写 `result_text` 辅助函数（见 `crates/minicoding-tools/src/web/fetch.rs` 第 148-153 行、`web/search.rs` 类似）。

#### 解决方案

**当前**：测试中用 pattern match 辅助函数（项目已有模式）：

```rust
fn result_text(result: &ToolResult) -> &str {
    match &result.content {
        ToolContent::Text(t) => t,
        _ => "",
    }
}
```

**建议改进**（需评审）：在 `crates/minicoding-core/src/model/tool.rs` 为 `ToolResult` 加 `text()` getter：

```rust
impl ToolResult {
    /// 返回文本内容（若 `content` 为 `Text`），否则空字符串。
    /// 测试与日志辅助方法。
    #[must_use]
    pub fn text(&self) -> &str {
        match &self.content {
            ToolContent::Text(t) => t,
            _ => "",
        }
    }
}
```

或为 `ToolContent` 加 `as_text()` 方法，避免与 `ToolContent::text()` 构造方法命名冲突。

此改进属于 `AGENTS.md` §7.6「保持简洁」的灰色地带——加 getter 减少测试样板，但增加了 API 表面。需在 PR 中讨论。

#### 预防措施

- 测试中统一用 `result_text` 辅助函数模式，复制现有测试模块的实现；
- 不要在测试中 `unwrap()` `ToolContent::Text`——用 pattern match 优雅处理非 Text 情况；
- 若新增工具的测试频繁需要提取文本，考虑在 PR 中推动加 `ToolResult::text()` getter；
- `ToolContent::text()` 是构造方法（`pub fn text(s: impl Into<String>) -> Self`），不要与潜在的 getter 混淆。

---

### 5.5 临时值被 drop（E0716）

#### 问题描述

编译报错：

```text
error[E0716]: cannot borrow `tmp.path()` as temporary value because it would be dropped
  --> crates/minicoding-cli/src/cred.rs:200:13
   |
200 |             std::env::set_var("MINICODING_HOME", tmp.path());
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
```

#### 原因分析

`tempfile::TempDir::path()` 返回 `&Path`（借用 `TempDir`），若 `TempDir` 是临时值（如 `tempfile::tempdir().unwrap().path()`），借用会在语句结束时 drop，`set_var` 接受 `AsRef<OsStr>` 但内部可能持有引用。

更常见场景：链式调用 `tempdir().unwrap().path().to_str().unwrap()` 把 `&str` 传给需要 `String`/`&str` 的函数，临时 `TempDir` 在表达式末尾 drop。

#### 解决方案

把 `TempDir` 绑定到变量，延长生命周期：

```rust
let tmp = tempfile::tempdir().expect("tempdir");
// SAFETY: 持有 ENV_LOCK 保证串行，无并发 set_var 风险。
unsafe { std::env::set_var("MINICODING_HOME", tmp.path()) };
// tmp 在作用域结束时 drop，删除临时目录
```

或转 `String` 解耦借用：

```rust
let home = tempfile::tempdir().expect("tempdir").path().to_owned();
unsafe { std::env::set_var("MINICODING_HOME", &home) };
```

注意 `TempDir` drop 时会删除临时目录，确保 `set_var` 后 `TempDir` 仍在作用域（用 `EnvGuard` 在 `Drop` 中恢复 env，`TempDir` 与 `EnvGuard` 同作用域）。

项目 `crates/minicoding-cli/src/cred.rs` 测试已用此模式（见第 198-200 行）。

#### 预防措施

- `TempDir`/`NamedTempFile` 总是绑定到变量，不链式调用 `.path()`；
- 借用 `&Path`/`&str` 时确认所有者生命周期覆盖借用使用期；
- `set_var` 接受 `AsRef<OsStr>`，传 `&str`/`String`/`&Path` 均可，但确保引用有效；
- `clippy::pedantic` 会 warn 部分 E0716 场景，注意修复。

---

### 5.6 clippy uninlined_format_args / manual_let_else

#### 问题描述

CI 的 `clippy` job 失败：

```text
error: uninlined format args
  --> crates/minicoding-tools/src/fs/read.rs:42:5
   |
42 |     format!("read {} bytes", n)
   |              ^^^^^^^^^^^^^^^^^^
   |
   = help: for further information visit https://rust-lang.github.io/rust-clippy/master/#uninlined_format_args
help: consider inlining the format args
   |
42 |     format!("read {n} bytes")
```

或：

```text
error: manual_let_else
  --> crates/minicoding-core/src/runtime.rs:120:5
   |
120 | /     let v = match opt {
121 | |         Some(x) => x,
122 | |         None => return None,
123 | |     };
   | |_____^ help: consider using `let else`
```

#### 原因分析

`clippy::pedantic` 包含 `uninlined_format_args`（建议 `format!("{n}")` 而非 `format!("{}", n)`）与 `manual_let_else`（建议 `let Some(x) = opt else { return ... }` 而非 `match`）。项目各 crate `lib.rs` 顶部 `#![deny(clippy::all, clippy::pedantic)]`（见 `AGENTS.md` §2.9），这些 lint 失败会阻断 CI。

`uninlined_format_args` 是 Rust 1.58+ 的内联格式化语法，更简洁；`manual_let_else` 是 `let-else`（Rust 1.65+）的惯用法，减少嵌套。

#### 解决方案

**uninlined_format_args**：把 `format!("{}", x)` 改为 `format!("{x}")`：

```rust
// 旧
format!("read {} bytes, truncated: {}", n, truncated)
// 新
format!("read {n} bytes, truncated: {truncated}")
```

复杂表达式仍需位置参数：

```rust
format!("{} + {} = {}", a, b, a + b)  // a + b 不能内联，保留
// 或
let sum = a + b;
format!("{a} + {b} = {sum}")
```

**manual_let_else**：把 `match` + `return` 改为 `let else`：

```rust
// 旧
let v = match opt {
    Some(x) => x,
    None => return None,
};

// 新
let Some(v) = opt else { return None };
```

批量修复：

```bash
cargo clippy --workspace --exclude minicoding-desktop --all-targets --all-features --fix --allow-dirty
cargo fmt --all  # 修复后重新格式化
```

#### 预防措施

- 新代码直接用内联格式化 `format!("{x}")` 与 `let else`；
- `cargo clippy --fix` 可自动修复大部分 pedantic lint；
- 不要用 `#![allow(clippy::uninlined_format_args)]` 全局放松（`AGENTS.md` §2.9 不用全局 allow）；
- 例外用 `#[allow(clippy::xxx)]` + 紧跟一行注释说明理由；
- 本地 pre-commit hook 会跑 `cargo clippy -D warnings`（见 `scripts/git-hooks/pre-commit`），提交前自查。

---

## 6. 权限与安全问题

### 6.1 路径越界（PathEscaped）

#### 问题描述

`fs.write`/`fs.read`/`fs.list` 工具调用报错：

```text
[fs.write] DENY: PathEscaped: /etc/passwd is outside workspace /home/user/project
```

或 LLM 试图通过 `../` 穿越到工作区外：

```text
[fs.read] DENY: PathEscaped: ../../../etc/shadow is outside workspace
```

#### 原因分析

所有文件工具输入经 `sandbox_path` 规范化校验（C-03，见 `AGENTS.md` §5.1、`docs/security.md` §3）。`sandbox_path` 用 `std::path::canonicalize` + `camino::Utf8PathBuf` 规范化路径，与 workspace root 比对，越界直接返回 `PathEscaped` 错误。

这是 L0 硬约束（C-03 路径不可越界），在实现层强制，不依赖 LLM 自觉。

#### 解决方案

**LLM 试图越界**：这是预期行为——`sandbox_path` 正确拦截了越界访问。检查 LLM 为什么需要越界：

- 若是合理需求（如读取系统配置），改用 `shell.run` + 用户显式 `--allow`（仍受黑名单约束）；
- 若是 LLM 误判，在 prompt 中明确工作区边界；
- 若是恶意 prompt injection，`<tool_output>` 边界（C-05）已防止工具结果注入指令。

**合法跨目录访问**：配置 `policy.toml` 显式 allow：

```toml
[[allow]]
tool = "fs.read"
[allow.match]
glob = "/etc/hosts"  # 允许读取特定系统文件
```

或用 `--allow "fs.read:/etc/hosts"` CLI 参数。

**符号链接越界**：`canonicalize` 会解析符号链接，指向工作区外的链接会被拦截。若需允许特定符号链接，在 `policy.toml` 显式 allow 目标路径。

#### 预防措施

- `sandbox_path` 是第一道防线，所有文件工具输入必经（C-03）；
- 不要为「方便」关闭路径沙箱——`full-access` 预设仍受 `sandbox_path` 约束（除非 `external-sandbox`）；
- `policy.toml` 的 `[[allow]]` 用最小权限原则，避免 `glob = "**"` 全放行；
- 定期 review `audit.log` 中的 `PathEscaped` 记录，发现 LLM 越界企图模式；
- 符号链接场景在 `mcp/approval.rs` 的 `project_fingerprint` 也有类似处理（见 `docs/review-report.md` §4.2 C1）。

---

### 6.2 黑名单被 Hook 覆盖风险

#### 问题描述

用户配置了 Hook 试图允许危险命令：

```toml
# .minicoding/hooks.toml
[[hooks.PreToolUse]]
command = "echo allow"  # Hook 输出 {"decision": "allow"}
matcher = "shell.run"
```

期望 Hook 的 `allow` 能覆盖 `rm -rf /` 等危险命令的黑名单。

#### 原因分析

`AGENTS.md` §5.1 与 C-21 规定：内置黑名单（`policy::builtin`）优先级最高，Hook 的 `allow` 对黑名单 `Deny` 无效。Hook 在 builtin 黑名单**之后**生效——黑名单 `Deny` 直接返回，不调 Hook。

这是与 Claude Code 的关键差异——CC 的 Hook 可覆盖黑名单（依赖自觉），minicoding-rs 的 L0 是编译期 + 运行期双重强制（见 `docs/getting-started.md` §2.2.3）。

#### 解决方案

**用户侧**：理解 Hook 不可覆盖黑名单是设计内行为。若需允许某命令：

1. 确认命令不在 builtin 黑名单（`rm -rf /`/`sudo`/`dd of=/dev/`/fork bomb 等，见 `docs/security.md` §4.2）；
2. 在 `policy.toml` 配置 `[[allow]]`（specificity=2，仍受 L0 约束）：
   ```toml
   [[allow]]
   tool = "shell.run"
   [allow.match]
   command_prefix = ["rm -rf target/"]  # 仅允许特定目录，黑名单 rm -rf / 仍 deny
   ```
3. 黑名单命令无法被任何配置允许——这是不可协商的 L0 约束。

**审计**：`audit.log` 记录所有决策（`Allow`/`Deny`/`Ask`/`AllowAlways`/`DenyAlways`），包括 Hook 触发的决策与黑名单 `Deny`（见 `AGENTS.md` §5.5）。

#### 预防措施

- builtin 黑名单（`policy/builtin.rs`）优先级最高，用户配置与 Hook 都无法覆盖（C-02、C-21）；
- Hook 的 `allow` 对黑名单 `Deny` 无效，Hook 仅能影响非黑名单的请求；
- `audit.log` 记录决策链（黑名单 → Hook → 用户策略 → 默认矩阵），便于追溯；
- 不要试图通过 Hook 绕过黑名单——改用 `policy.toml` 的 `[[allow]]`，仍受 L0 约束；
- 安全审查时检查 `hooks.toml` 是否有可疑 Hook（如 `matcher = "shell.run"` + `allow`），但理解它们无法绕过黑名单。

---

### 6.3 凭证泄露防护

#### 问题描述

测试或日志中担心 API key 泄露：

- `fs.read` 读取 `.env` 文件时会不会把 key 输出给 LLM？
- 日志中 `Authorization: Bearer sk-xxx` 会不会明文记录？
- 子进程（shell.run/MCP/Hook）能不能通过 `env` 拿到 API key？

#### 原因分析

C-04（凭证不可外泄，见 `AGENTS.md` §5.1、`docs/security.md` §6）规定：

- 凭证仅存内存与 OS keyring，不下传子进程 env；
- 日志中密钥脱敏（前 4 字符 + `***`）；
- `.env`/`credentials`/`*.pem`/`*.key` 等敏感文件 `fs.read` 自动脱敏。

实现位置：

- `crates/minicoding-tools/src/fs/read.rs::is_sensitive_path`：检测敏感文件；
- `crates/minicoding-policy/src/redact.rs`：脱敏逻辑（前 4 字符 + `***`）；
- 子进程 env 白名单/黑名单：`shell_environment_policy`（见 `docs/security.md` §10）。

#### 解决方案

**验证脱敏生效**：

```bash
minicoding "读取 .env 文件"
# 输出应显示 sk-x*** 而非完整 key
```

**检查日志脱敏**：

```bash
# ~/.minicoding/logs/minicoding.YYYY-MM-DD.log
grep -i "sk-" ~/.minicoding/logs/*.log
# 应只出现 sk-x*** 格式，无完整 key
```

**子进程 env 隔离**：

```toml
# ~/.minicoding/config.toml
[shell_environment_policy]
mode = "denylist"  # 或 "allowlist"
denylist = ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "AWS_*", "*_TOKEN"]
# 或
allowlist = ["PATH", "HOME", "LANG"]
```

子进程（`shell.run`/MCP server/Hook）的 env 经 `shell_environment_policy` 过滤，API key 默认不下传。

**测试用 mock 凭证**：

```rust
// 测试中用 mock 凭证，不写真实格式
let key = "sk-test-xxxxxxxx";
```

CI 的 `OPENAI_API_KEY=sk-test-ci-mock-not-real`（见 `ci.yml` 第 69 行）。

#### 预防措施

- 凭证只从环境变量或 OS keyring 读，**绝不**硬编码在源码/测试/文档（`AGENTS.md` §5.3）；
- 测试用 mock 凭证（`sk-test-xxx`），不写真实格式；
- 日志中密钥脱敏由 `policy::redact` 强制，不要在业务代码手动 `println!("{}", key)`；
- `shell_environment_policy` 配置严格白名单/黑名单，防止子进程拿到 key；
- `fs.read` 敏感文件自动脱敏，不要为「方便」关闭 `is_sensitive_path` 检查；
- `audit.log` 0600 权限 + fsync，防止凭证泄露到审计日志；
- 不提交 `.env`/`credentials.json`/`*.pem`/`*.key`（`AGENTS.md` §6.4，pre-commit hook 检查）。

---

### 6.4 AGENTS.md 被 Agent 编辑风险

#### 问题描述

LLM 试图通过 `fs.write`/`fs.edit` 修改 `AGENTS.md` 或 `CLAUDE.md`，期望「优化提示词」或「记住新规则」。

#### 原因分析

C-23（AGENTS.md 不可被 Agent 自主编辑，见 `AGENTS.md` §5.1、`docs/security.md` §2）规定：对 `AGENTS.md`/`CLAUDE.md` 写操作注入 `Verdict::Ask` 且不可 `AllowAlways`。

这是防止 Agent 通过修改项目记忆文件越权——若 Agent 能写 `AGENTS.md`，就能写入「允许所有命令」「禁用沙箱」等指令，下次启动时绕过约束。

实现位置：`crates/minicoding-policy/src/builtin.rs`，对 `fs.write`/`fs.edit` 的目标路径匹配 `AGENTS.md`/`CLAUDE.md`/`.cursorrules` 时强制 `Ask`，`AllowAlways` 选项不出现。

#### 解决方案

**Agent 试图编辑 AGENTS.md**：

1. 权限弹窗显示 `Ask`（无 `Always allow` 选项）；
2. 用户选择 `Allow` 仅本次生效，下次仍 `Ask`；
3. 用户选择 `Deny` 拒绝；
4. `audit.log` 记录决策。

**Agent 需要记忆新信息**：用 `long_term.md`（Agent 可写）而非 `AGENTS.md`（Agent 不可写）：

```text
minicoding> 用 memory.write 工具记住「这个项目用 tabs 而非 spaces」
```

`long_term.md` 与 `AGENTS.md` 物理隔离（C-27，见 `docs/security.md` §2、`docs/data-model.md` §6.4 对比表）。

**Auto memory 指令性内容降级**：`auto.md` 若含指令性内容（如「允许 rm -rf」）降级为 `Ask`（C-27），防止通过 auto memory 越权。

#### 预防措施

- `AGENTS.md`/`CLAUDE.md`/`.cursorrules` 写操作强制 `Ask`，不可 `AllowAlways`（C-23）；
- 动态记忆写 `long_term.md`/`auto.md`，不写 `AGENTS.md`；
- `auto.md` 指令性内容降级 `Ask`（C-27），`long_term.md` 与 `auto.md` 物理隔离；
- `audit.log` 记录所有 `AGENTS.md` 写尝试，便于发现 Agent 越权企图；
- 安全审查时检查 `audit.log` 中 `AGENTS.md` 相关的 `Ask`/`Deny` 记录；
- 不要为「方便」在 `policy.toml` 配置 `[[allow]] tool = "fs.write" glob = "AGENTS.md"`——builtin 黑名单优先级最高，该配置无效（C-02）。

---

## 7. MCP 与 Hook 问题

### 7.1 MCP project 作用域恶意仓库植入

#### 问题描述

clone 一个含恶意 `.minicoding/mcp.json` 的仓库，进入后 `minicoding` 自动连接恶意 MCP server，泄露本地文件或执行危险命令。

#### 原因分析

MCP server 配置有三个作用域（`docs/data-model.md` §6.4）：

- `local`：仅当前用户，最安全；
- `user`：用户全局，次之；
- `project`：仓库内 `.minicoding/mcp.json`，**最危险**——clone 即植入。

C-24（MCP project 作用域 server 必须经首次批准，见 `AGENTS.md` §5.1、`docs/security.md` §11）规定：含 `.minicoding/mcp.json` 的仓库首次进入逐个 server 弹窗批准，未批准不连接不注册。

#### 解决方案

**首次进入仓库**：

```text
[permission] MCP project server "github" wants to start
  command: npx -y @modelcontextprotocol/server-github
  scope: project (from .minicoding/mcp.json)
  Risk: HIGH (project-scoped MCP, review before approve)
  [y] Approve  [n] Deny  [e] Explain
```

逐个 server 审查后批准或拒绝。批准记忆存 `mcp_choices.toml`，按项目路径指纹分桶（`crates/minicoding-mcp/src/approval.rs`，原子写 `.tmp` + `rename`）。

**重置批准记忆**：

```bash
minicoding mcp reset-project-choices
# 删除当前项目的 mcp_choices.toml 条目，下次重新弹窗
```

**审查 mcp.json**：

```bash
cat .minicoding/mcp.json
# 检查每个 server 的 command/args/env，确认无恶意行为
```

**恶意仓库处理**：

```bash
# 拒绝所有 server，删除 mcp.json
rm .minicoding/mcp.json
# 或退出仓库
cd ..
```

#### 预防措施

- project 作用域 MCP server 首次必弹窗批准（C-24），未批准不连接；
- `mcp_choices.toml` 按项目指纹分桶，符号链接场景下 `canonicalize` 失败回退原始路径（见 `docs/review-report.md` §4.2 C1，建议补注释说明指纹稳定性）；
- 批准记忆原子写（`.tmp` + `rename`），防止并发写损坏；
- MCP server 的 env 经 `shell_environment_policy` 过滤（C-04），不下传 API key；
- MCP server 在沙箱内运行（C-22、C-26），路径访问受 `sandbox_path` 约束（C-03）；
- MCP 工具命名 `mcp__<server>__<tool>`，`side_effect` 据 schema hint 映射（C-25），未声明 `readOnlyHint`/`destructiveHint` 默认 `SideEffect::Command`（串行 + Ask）；
- clone 陌生仓库后先 `cat .minicoding/mcp.json` 审查，再 `minicoding mcp list` 查看待批准 server。

---

### 7.2 asyncRewake 越权风险

#### 问题描述

Hook 的 `asyncRewake` 协议被滥用：后台 Hook 子进程在用户不知情时执行高权限操作（如 `fs.write`/`shell.run`），绕过权限检查。

#### 原因分析

`asyncRewake` 允许 Hook 在 `PostToolUse`/`PostToolUseFailure`/`Stop` 事件后异步唤醒 Runtime 执行后续操作（`docs/hooks.md` §11）。若不约束，后台 Hook 可能：

- 越权执行副作用工具；
- 访问凭证（C-04）；
- 越界写文件（C-03）。

C-26（asyncRewake 不可越权，见 `AGENTS.md` §5.1、`docs/hooks.md` §11）规定：后台 Hook 子进程遵守凭证隔离（C-04）、沙箱（C-22）、路径沙箱（C-03）。

#### 解决方案

**asyncRewake 约束**（已在实现层强制）：

1. **事件白名单**：仅 `PostToolUse`/`PostToolUseFailure`/`Stop` 有效，其他事件忽略 asyncRewake；
2. **并发上限**：3 个并发 asyncRewake，超时 kill（`default_timeout_sec`，默认 30s）；
3. **权限**：asyncRewake 触发的工具调用仍走 `PermissionPolicy::check`，受 L0 黑名单（C-02）、路径沙箱（C-03）、凭证隔离（C-04）约束；
4. **沙箱**：后台 Hook 子进程在沙箱内运行（C-22）；
5. **审计**：asyncRewake 协议错误、Hook 协议违规均记 `audit.log`（`AGENTS.md` §5.5）。

**配置 asyncRewake**：

```toml
# .minicoding/hooks.toml
[[hooks.PostToolUse]]
command = "./scripts/post_tool.sh"
matcher = "fs.write|fs.edit"
async_rewake = true  # 启用 asyncRewake
timeout_sec = 20
```

**审查 Hook 脚本**：

```bash
cat scripts/post_tool.sh
# 检查脚本内容，确认无危险操作
```

#### 预防措施

- asyncRewake 仅对白名单事件有效（C-26），不要试图在其他事件启用；
- 后台 Hook 子进程权限与前台一致，不因「后台」放松；
- `default_timeout_sec` 限制 Hook 执行时间，超时 kill 防止僵尸进程；
- 3 并发上限防止资源耗尽（C-07）；
- `audit.log` 记录 asyncRewake 调用与错误，便于追溯；
- 安全审查时检查 `hooks.toml` 的 `async_rewake = true` 配置，理解后台 Hook 行为；
- Hook 脚本不要硬编码凭证（C-04），用 `env:VAR_NAME` 引用。

---

### 7.3 Hook 超时处理

#### 问题描述

Hook 脚本执行超时（如 `cargo fmt` 在大项目上耗时超过 `timeout_sec`），Runtime 卡住或报错。

#### 原因分析

Hook 配置有 `timeout_sec`（默认 30s，见 `docs/hooks.md` §6）。超时后 Runtime kill Hook 子进程，按 `on_hook_error` 策略处理（`continue`/`abort`，默认 `continue`）。

常见超时场景：

- `cargo fmt`/`cargo clippy` 在大项目耗时长；
- Hook 脚本等待网络（应避免，Hook 应快速）；
- Hook 脚本死锁/无限循环。

#### 解决方案

**调整 timeout_sec**：

```toml
# .minicoding/hooks.toml
[hooks]
default_timeout_sec = 60  # 全局默认

[[hooks.PostToolUse]]
command = "cargo fmt"
matcher = "fs.write|fs.edit"
timeout_sec = 120  # 单个 Hook 覆盖，大项目给足时间
```

**on_hook_error 策略**：

```toml
[hooks]
on_hook_error = "continue"  # 默认，Hook 失败继续 Agent 循环
# 或
on_hook_error = "abort"  # Hook 失败中止 Agent 循环
```

`continue` 适合非关键 Hook（如格式化），`abort` 适合关键 Hook（如安全检查）。

**优化 Hook 脚本**：

- Hook 应快速（< 10s），重操作放 asyncRewake 或独立任务；
- 避免在 Hook 中等待网络；
- Hook 脚本用 `set -euo pipefail` 快速失败；
- 大项目 `cargo fmt` 改为只格式化变更文件：`git diff --name-only | xargs rustfmt`。

**超时后排查**：

```bash
# 查看 audit.log 中的 Hook 超时记录
grep "hook_timeout" ~/.minicoding/audit.log
```

#### 预防措施

- `default_timeout_sec = 30` 是合理默认，重操作 Hook 单独调大 `timeout_sec`；
- `on_hook_error = "continue"` 不阻断 Agent 循环，`abort` 适合安全关键 Hook；
- Hook 脚本应幂等（多次执行结果一致），超时重试不破坏状态；
- asyncRewake 的超时独立于同步 Hook（C-26，3 并发上限 + 超时 kill）；
- `audit.log` 记录 Hook 超时与错误，便于定位慢 Hook；
- 定期 review `hooks.toml`，移除不再需要的 Hook，避免超时累积。

---

## 8. 前端与桌面问题

### 8.1 Tauri feature 隔离

#### 问题描述

`cargo build --workspace --all-features` 因 Tauri 系统库缺失失败（见 §2.3），或 `cargo deny` 因 Tauri 传递依赖许可证不在白名单失败。

#### 原因分析

`minicoding-desktop` 的 `desktop` feature 依赖 Tauri 2.x，传递大量 GUI 依赖（见 §2.2、§2.3）。`AGENTS.md` §3.5 规定重依赖通过 feature gate 隔离在对应实现 crate，`desktop` feature 默认关闭。

CI 与 pre-commit hook 均 `--exclude minicoding-desktop`，`deny.toml` 的 `[graph] exclude = ["minicoding-desktop"]` 排除许可证检查。

#### 解决方案

**常规开发**：

```bash
cargo build --workspace --exclude minicoding-desktop
cargo clippy --workspace --exclude minicoding-desktop --all-features -- -D warnings
cargo test --workspace --exclude minicoding-desktop --all-features
cargo deny check advisories licenses bans sources  # deny.toml 已 exclude
```

**桌面开发**：

```bash
# 先装 Tauri 系统库（见 §2.2）
cargo build -p minicoding-desktop --features desktop
cargo clippy -p minicoding-desktop --features desktop -- -D warnings
```

**deny.toml 配置**：

```toml
[graph]
exclude = ["minicoding-desktop"]  # Tauri 传递依赖许可证不在白名单
all-features = true
```

#### 预防措施

- `desktop` feature 默认关闭，不加入 workspace `default-features`；
- CI 的 `desktop` job 单独安装系统库后编译（见 `ci.yml` 第 161-175 行）；
- `deny.toml` 排除 `minicoding-desktop` 的依赖治理，但 `minicoding-desktop` 自身代码仍受 `clippy`/`fmt` 约束；
- 前端 `minicoding-web` 是独立 npm 项目（`AGENTS.md` §8.2），不属于 Cargo workspace，不影响 cargo 构建；
- 改动 `minicoding-desktop` 时本地先验证 `cargo build -p minicoding-desktop --features desktop` 通过。

---

### 8.2 CORS 配置

#### 问题描述

浏览器访问 `http://localhost:3000` 的前端，请求 `http://localhost:8080` 的 `minicoding-server` 时被 CORS 拦截：

```text
Access to fetch at 'http://localhost:8080/sessions' from origin 'http://localhost:3000'
has been blocked by CORS policy: No 'Access-Control-Allow-Origin' header is present.
```

#### 原因分析

Web 模式下浏览器跨域请求 `minicoding-server`，需服务端配置 CORS（`AGENTS.md` §8.6、`docs/design.md` §24）。`minicoding-server` 用 `tower-http::cors` 提供 CORS 支持，默认仅允许 `http://localhost:*` 来源。

#### 解决方案

**启动 server 时配置 CORS**：

```bash
# 允许特定来源
minicoding serve --http --cors-origin http://localhost:3000

# 允许多个来源
minicoding serve --http --cors-origin http://localhost:3000,http://localhost:5173

# 允许所有来源（仅开发环境，生产禁用）
minicoding serve --http --cors-origin "*"
```

**或配置文件**：

```toml
# ~/.minicoding/config.toml
[server]
cors_origins = ["http://localhost:3000", "https://minicoding.example.com"]
```

**SSE 跨域**：SSE 事件流同样受 CORS 约束，`tower-http::cors` 已覆盖 SSE 端点。

**预检请求**：浏览器对 `POST /sessions/{id}/messages` 等非简单请求发 `OPTIONS` 预检，`tower-http::cors` 自动响应。

#### 预防措施

- 生产环境 `cors_origins` 配置具体域名，不用 `"*"`；
- 开发环境 `http://localhost:*` 覆盖 Vite 默认端口（5173）与自定义端口；
- SSE 端点 `/sessions/{id}/events` 的 CORS 与 HTTP 端点一致；
- 权限交互（`PermissionPrompt` 经 SSE 推送，决策经 JSON-RPC 回传）同样受 CORS 约束（`AGENTS.md` §8.6）；
- Tauri 桌面模式不走 CORS（前端与 sidecar 同进程组，IPC 不跨域）；
- CORS 不替代认证——生产环境 `minicoding-server` 应加认证层（如 token/API key 中间件）。

---

### 8.3 SSE 事件增量更新

#### 问题描述

前端接收 SSE 事件流时，token 增量更新卡顿、重复渲染，或 `MessageAppended` 事件触发整条消息重新请求（refetch）导致闪烁。

#### 原因分析

`AGENTS.md` §8.5 规定：

- 服务端状态（会话、消息、任务）用 TanStack Query；
- 流式状态（token 增量、工具进度）用 `queryClient.setQueryData` 增量更新缓存；
- `Token` 事件追加到消息末尾，`MessageAppended` 事件替换整条消息；
- **不触发 refetch**——refetch 会导致闪烁与无谓网络请求。

常见错误：

1. `Token` 事件触发 `queryClient.invalidateQueries`（导致 refetch）；
2. `MessageAppended` 事件未更新缓存，等下次 refetch；
3. 多个 `Token` 事件并发更新缓存，无乐观锁导致丢更新；
4. SSE 连接断开后未重连，token 流中断。

#### 解决方案

**Token 事件增量更新**：

```typescript
// hooks/useSessionStream.ts
useQuery({
  queryKey: ['session', sessionId, 'events'],
  queryFn: () => subscribeSSE(sessionId, (event) => {
    switch (event.type) {
      case 'Token':
        // 增量追加到消息末尾，不 refetch
        queryClient.setQueryData(['session', sessionId, 'messages'], (old: Message[]) => {
          const last = old[old.length - 1];
          return [...old.slice(0, -1), { ...last, content: last.content + event.delta }];
        });
        break;
      case 'MessageAppended':
        // 替换整条消息
        queryClient.setQueryData(['session', sessionId, 'messages'], (old: Message[]) => {
          const idx = old.findIndex((m) => m.id === event.message.id);
          if (idx >= 0) {
            const next = [...old];
            next[idx] = event.message;
            return next;
          }
          return [...old, event.message];
        });
        break;
      case 'PermissionRequest':
        // 弹出 Dialog，不走 query 缓存
        permissionStore.openPrompt(event.prompt);
        break;
    }
  }),
});
```

**SSE 重连**：

```typescript
// 用 EventSource 自动重连，或自实现指数退避
const es = new EventSource(`/sessions/${sessionId}/events`);
es.onerror = () => {
  // EventSource 会自动重连，无需手动处理
};
```

**避免 refetch**：

- 不要在 `Token` 事件后 `invalidateQueries`；
- `MessageAppended` 用 `setQueryData` 替换，不 refetch；
- 仅在用户主动操作（如刷新、切换会话）时 `invalidateQueries`。

#### 预防措施

- 服务端状态走 TanStack Query，客户端 UI 状态走 Zustand，**不混用**（`AGENTS.md` §8.5）；
- SSE 事件用 `setQueryData` 增量更新，不 `invalidateQueries`；
- `Token` 追加、`MessageAppended` 替换，事件语义明确；
- SSE 连接用 `EventSource`（自动重连）或自实现指数退避；
- 测试用 MSW 拦截 SSE（`AGENTS.md` §8.8），不连真实后端；
- Zod parse SSE 事件后再生效，防止后端 schema 漂移（`AGENTS.md` §8.4）。

---

### 8.4 CSP 严格策略

#### 问题描述

Tauri 桌面模式下前端加载远程 CDN 资源被 CSP 拦截：

```text
Refused to load the script 'https://cdn.example.com/lib.js'
because it violates the following Content Security Policy directive: "default-src 'self'".
```

或 Web 模式下 XSS 攻击通过 `dangerouslySetInnerHTML` 注入恶意脚本。

#### 原因分析

`AGENTS.md` §8.9 规定：

- Tauri 桌面模式默认禁用远程内容，CSP 严格（仅允许 `self` + sidecar origin）；
- Web 模式由部署侧配置 CSP header；
- XSS 防护：流式 token 渲染用 React `{text}` 转义，**禁用** `dangerouslySetInnerHTML`（除非经 DOMPurify 清洗，仅用于 Markdown 渲染）。

#### 解决方案

**Tauri CSP 配置**：

```json
// crates/minicoding-desktop/tauri.conf.json
{
  "tauri": {
    "security": {
      "csp": "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; connect-src 'self' ipc: http://ipc.localhost"
    }
  }
}
```

仅允许 `self` 与 sidecar IPC，禁用所有远程内容。

**Web 模式 CSP header**（部署侧）：

```nginx
# nginx.conf
add_header Content-Security-Policy "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:;" always;
```

**Markdown 渲染 XSS 防护**：

```typescript
import DOMPurify from 'dompurify';
import { marked } from 'marked';

function Markdown({ content }: { content: string }) {
  // marked 解析 + DOMPurify 清洗，再用 dangerouslySetInnerHTML
  const html = DOMPurify.sanitize(marked.parse(content) as string);
  return <div dangerouslySetInnerHTML={{ __html: html }} />;
}

// 普通文本渲染用 React 转义，不用 dangerouslySetInnerHTML
function Token({ text }: { text: string }) {
  return <span>{text}</span>;  // React 自动转义
}
```

**禁止**：

- 不要在普通 token 渲染用 `dangerouslySetInnerHTML`；
- 不要在 CSP 中加 `unsafe-eval`（除非 Vite 开发模式需要，生产移除）；
- 不要加载远程 CDN 脚本（用 npm 包 + 本地构建）。

#### 预防措施

- Tauri CSP 默认严格（`default-src 'self'`），禁用远程内容；
- Web 模式部署侧配 CSP header，生产禁用 `unsafe-eval`/`unsafe-inline`（除 style）；
- 流式 token 用 React `{text}` 转义，Markdown 用 `DOMPurify.sanitize` 后再 `dangerouslySetInnerHTML`；
- 不加载远程 CDN，所有依赖走 npm + 本地构建；
- 凭证不存前端（Web 模式由 server 持有，桌面用 OS keyring，C-04，`AGENTS.md` §8.9）；
- 不持久化消息到 `localStorage`/`IndexedDB`（会话日志由后端管理）；
- 权限决策不前端短路（前端「允许/拒绝」仅回传 Decision，后端强制 C-01）。

---

## 9. 性能问题

### 9.1 冷启动优化

#### 问题描述

`minicoding --version` 或 `minicoding "简单问题"` 的冷启动耗时 > 100ms，用户感知「卡」。

#### 原因分析

`docs/design.md` §13 性能预算规定冷启动 < 50ms。冷启动慢的常见原因：

1. 重依赖初始化（如 `reqwest`/`tokio` runtime 启动）；
2. 配置全量加载（而非惰性）；
3. AGENTS.md/记忆文件同步读取；
4. OTel SDK 初始化；
5. `clap` 参数解析（derive 宏开销）。

#### 解决方案

**惰性加载**：

- 配置按需加载（`RuntimeConfig` 分层，只在需要时读 `~/.minicoding/config.toml`）；
- 记忆文件 mtime 缓存（`memory/auto.rs`），无变更零 IO/分词；
- OTel SDK 仅在 `OTEL_EXPORTER_OTLP_ENDPOINT` 设置时初始化，否则降级本地 fmt 日志；
- AGENTS.md 32KiB 截断（`project_doc_max_bytes`），避免大文件全读。

**减少重依赖初始化**：

- `tokio::runtime` 用 `current_thread`（CLI 单线程足够，避免多线程 runtime 启动开销）；
- `reqwest::Client` 惰性创建（首次 LLM 调用时）；
- `tiktoken-rs` 分词器惰性加载（首次 token 计数时）。

**测量**：

```bash
# 用 hyperfine 测冷启动
hyperfine --warmup 3 'minicoding --version'
# 或 time
time minicoding --version
```

**OTel 采样**：

```bash
# 生产用 TraceIdRatio 采样，降低导出开销
export OTEL_TRACES_SAMPLER="traceidratio"
export OTEL_TRACES_SAMPLER_ARG="0.1"  # 10% 采样
```

#### 预防措施

- 冷启动目标 < 50ms（`docs/design.md` §13）；
- 重依赖惰性初始化，不在 `main` 入口同步加载；
- 配置分层加载（CLI args > env > project > user > defaults），按需读取；
- 记忆文件 mtime 缓存（`memory/auto.rs`），无变更零 IO；
- OTel SDK 仅在配置导出端点时初始化；
- `cargo build --release` 用 `lto = "thin"`（`Cargo.toml` `[profile.release]`）优化二进制；
- 性能基准用 `criterion`（`docs/tech-stack.md` §10），CI 监控回归。

---

### 9.2 流式首 token 延迟

#### 问题描述

LLM 响应的首 token 延迟 > 2ms（本地 Ollama）或 > 500ms（远端 OpenAI），用户感知「等很久才看到输出」。

#### 原因分析

`docs/design.md` §13 性能预算规定流式首 token 转发 < 2ms（本地）。延迟来源：

1. **网络**：远端 provider RTT（OpenAI ~200-500ms，Anthropic 类似）；
2. **事件总线**：`Token` 事件经 `EventBus` 广播，若中间缓冲则延迟；
3. **前端**：SSE 事件经 TanStack Query `setQueryData`，若触发 refetch 则延迟；
4. **OTel 导出**：每个 span 导出若同步则阻塞；
5. **token 计数**：`tiktoken-rs` 同步计数若在 token 流路径上则阻塞。

#### 解决方案

**事件总线直通**：

- `EventBus` 直通无中间缓冲（`docs/design.md` §13 设计保障）；
- `Token` 事件用 `broadcast` 通道，订阅者立即收到。

**前端增量更新**（见 §8.3）：

- `Token` 事件用 `setQueryData` 增量追加，不 refetch；
- SSE 连接用 `EventSource`（浏览器原生，低延迟）。

**OTel 异步导出**：

- OTel exporter 用 `rt-tokio` 异步导出（`opentelemetry_sdk` feature，见 `Cargo.toml` 第 53 行）；
- span 创建不阻塞业务线程。

**token 计数移出热路径**：

- 流式 token 计数在 turn 结束后批量统计，不在每个 token 上同步计数；
- 或用启发式估算（`cl100k` 近似），turn 结束后精确修正。

**测量**：

```bash
# 本地 Ollama 测首 token 延迟
time minicoding --provider ollama "hello"
# OTel trace 可见 llm_call span 的 first_token 时间
```

**远端 provider 优化**：

- 用 `reqwest` 连接池（`Client` 复用，避免 TLS 握手开销）；
- 流式响应用 `eventsource-stream` 增量解析，不等待完整响应。

#### 预防措施

- 首 token 转发目标 < 2ms（本地，`docs/design.md` §13）；
- 事件总线直通无缓冲，`Token` 事件 `broadcast` 即时；
- 前端 `setQueryData` 增量更新，不 refetch（§8.3）；
- OTel 异步导出，不阻塞业务线程；
- token 计数移出热路径，turn 结束后批量统计；
- `reqwest::Client` 复用连接池，避免重复 TLS 握手；
- 性能基准用 `criterion` 监控首 token 回归；
- 远端 provider 延迟主要来自网络，本地 Ollama 是延迟下限参考。

---

### 9.3 万级会话列出性能

#### 问题描述

`minicoding session list` 在积累万级会话后耗时 > 1s，或 TUI 会话侧栏加载卡顿。

#### 原因分析

`docs/dev-plan.md` T-M3-4 规定万级会话列出 < 1s，`docs/features.md` A-14（64KB 窗口会话列出）实现机制：

- 每个会话 JSONL 文件有 `index.json` 侧车文件（轻量元数据）；
- `session list` 不读完整 JSONL，只读 `index.json`；
- 64KB 窗口：首尾各读 64KB，快速预览会话内容。

性能瓶颈：

1. `index.json` 文件多，IO 开销大；
2. 全量扫描目录；
3. 同步阻塞 IO；
4. 未用 mtime 缓存。

#### 解决方案

**session list 不构建 Runtime**：

`minicoding session list`/`delete` 子命令不构建 `Runtime`（避免 provider/storage 全量初始化），直接复用 `JsonlStorage` 同步方法，无需 API key（见 `docs/getting-started.md` §4.5）。

**index.json 轻量元数据**：

```rust
// crates/minicoding-storage/src/index.rs
// index.json 含 last_compaction_id/turn_count/summary 等，不读完整 JSONL
```

**64KB 窗口**：

```rust
// 首尾各读 64KB，快速预览
fn read_session_preview(path: &Path) -> SessionPreview {
    let mut file = File::open(path).expect("open");
    let head = read_head(&mut file, 64 * 1024);
    let tail = read_tail(&mut file, 64 * 1024);
    SessionPreview { head, tail }
}
```

**mtime 缓存**：

- `memory/auto.rs` 的 mtime 缓存模式可借鉴——`session list` 缓存目录扫描结果，目录 mtime 未变时复用缓存。

**异步并行 IO**：

```rust
// 用 tokio::task::spawn_blocking 并行读 index.json
let previews = futures::future::join_all(
    session_files.iter().map(|f| tokio::task::spawn_blocking(move || read_index(f)))
).await;
```

**测量**：

```bash
# 造万级会话
for i in $(seq 1 10000); do
    minicoding --session "test $i" <<< "exit"
done
time minicoding session list
# 应 < 1s
```

#### 预防措施

- 万级会话列出 < 1s（`docs/dev-plan.md` T-M3-4）；
- `index.json` 侧车文件轻量，不读完整 JSONL；
- 64KB 窗口预览（`docs/features.md` A-14）；
- `session list`/`delete` 不构建 Runtime，无需 API key；
- mtime 缓存避免重复扫描；
- 异步并行 IO 提升吞吐；
- `--resume` 读 `index.json` 的 `last_compaction_id` 定位起始行，避免全文件扫描（`docs/data-model.md` §3.1、§3.3）；
- 跨进程文件锁（`fs2`）防止两个进程同时写同一会话（`docs/data-model.md` §10）；
- 性能基准用 `criterion`（`docs/tech-stack.md` §10）监控回归。

---

## 10. 调试技巧

### 10.1 日志查看

#### 问题描述

运行出错时不知如何查看详细日志，或日志中密钥未脱敏。

#### 解决方案

**日志位置**：

```bash
# 默认本地 fmt 日志
~/.minicoding/logs/minicoding.YYYY-MM-DD.log

# 用 MINICODING_HOME 覆盖
export MINICODING_HOME=/custom/path
# 日志在 /custom/path/logs/
```

**日志级别**：

```bash
# 环境变量控制
export RUST_LOG=debug  # 全局 debug
export RUST_LOG=minicoding_core=trace,minicoding_providers=debug  # 细粒度
minicoding "..."
```

**实时查看**：

```bash
tail -f ~/.minicoding/logs/minicoding.$(date +%Y-%m-%d).log
```

**审计日志**：

```bash
# 权限决策审计（0600 权限，追加写不可篡改）
cat ~/.minicoding/audit.log
# 含 Allow/Deny/Ask/AllowAlways/DenyAlways 决策
```

**密钥脱敏**：

- `policy::redact` 自动脱敏（前 4 字符 + `***`）；
- 日志中不会出现完整 API key；
- 若发现完整 key 泄露，是 bug，按 §6.3 排查。

**会话日志**：

```bash
# JSONL 会话日志
ls ~/.minicoding/sessions/
# sess_01H....jsonl
cat ~/.minicoding/sessions/sess_01H....jsonl | jq .
```

#### 预防措施

- 日志默认本地 fmt，OTel 导出由 `OTEL_EXPORTER_OTLP_ENDPOINT` 控制（见 §10.2）；
- `RUST_LOG` 控制级别，生产用 `info`，调试用 `debug`/`trace`；
- `audit.log` 0600 权限 + fsync，不可篡改（`AGENTS.md` §5.5）；
- 密钥脱敏由 `policy::redact` 强制，不在业务代码手动 `println!("{}", key)`；
- 会话 JSONL 含完整消息历史，注意不要泄露给第三方。

---

### 10.2 OTel trace 分析

#### 问题描述

Agent 行为异常（如工具调用慢、权限决策错），但本地日志不够结构化，难以定位瓶颈。

#### 解决方案

**启动 OTel 导出**：

```bash
# 启动本地 collector（Jaeger/Tempo/Grafana）
docker run -d -p 4317:4317 -p 16686:16686 jaegertracing/all-in-one

# 配置 minicoding 导出
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
export OTEL_TRACES_SAMPLER="always_on"  # 调试用全采样
minicoding "..."
```

**查看 trace**：

```bash
# 浏览器打开 Jaeger UI
open http://localhost:16686
# 搜索 service = minicoding，查看 trace
```

**span 层级**（`docs/architecture.md` §7.3）：

```text
session
└── turn
    ├── llm_call (provider, model, token_in, token_out, latency)
    ├── tool_call (tool_name, side_effect, parallel, latency, result_size)
    │   ├── permission (verdict, source)
    │   └── sandbox (driver, hardened)
    ├── compress (level, before_tokens, after_tokens)
    └── hook.run (event, command, timeout)
```

每个 span 记录关键属性：

- `llm_call`：provider、model、token_in/out、latency；
- `tool_call`：tool_name、side_effect、是否并行、耗时、结果大小；
- `permission`：verdict、决策来源（黑名单/Hook/用户策略）；
- `compress`：压缩级别、压缩前后 token 数；
- `hook.run`：事件、命令、超时。

**性能瓶颈定位**：

- 找耗时最长的 span（如 `llm_call` 5s）；
- 看 span 属性（如 `token_out=10000` 说明 LLM 输出过长）；
- 看父子关系（如 `tool_call` 串行 vs 并行）。

**生产采样**：

```bash
# 生产用 TraceIdRatio 采样，降低导出开销
export OTEL_TRACES_SAMPLER="traceidratio"
export OTEL_TRACES_SAMPLER_ARG="0.1"  # 10%
```

#### 预防措施

- OTel 是一等公民（M0 起接入，`docs/tech-stack.md` §7），所有跨组件边界打 span；
- 业务代码只写 `tracing` 宏，subscriber 层同时输出本地日志与 OTLP trace，无重复埋点；
- 调试用 `always_on` 全采样，生产用 `traceidratio` 采样；
- span 属性记录关键信息（工具名、side_effect、verdict、latency），便于分析；
- OTel exporter 异步导出（`rt-tokio`），不阻塞业务线程；
- 后端由标准环境变量控制（`OTEL_EXPORTER_OTLP_ENDPOINT`/`OTEL_TRACES_SAMPLER`），零代码改动切换。

---

### 10.3 doctor 自检

#### 问题描述

不确定沙箱、权限、凭证配置是否正确，或运行时行为异常需自检。

#### 解决方案

**全量自检**：

```bash
minicoding doctor
# 检查：Rust 工具链、配置文件、凭证状态、沙箱驱动、权限配置
```

**安全自检**：

```bash
minicoding doctor --security
# 输出：
# Sandbox Driver: LandlockDriver (Linux 5.13+)
# Hardened: true
# VCS Protection: .git/.hg/.svn 写保护
# Permission Config: policy.toml 加载状态
# Credential: keyring/env 状态
```

**关键检查项**：

| 检查 | 期望 | 异常处理 |
|------|------|---------|
| Sandbox Driver | `LandlockDriver`/`SeatbeltDriver`/`WindowsJobObjectDriver` | `NoopDriver` 见 §4.5 |
| Hardened | `true` | `false` 见 §4.3/§4.4/§4.5 |
| VCS Protection | `.git`/`.hg`/`.svn` 写保护 | 未保护检查 `policy.toml` |
| Credential | keyring 或 env 已配置 | 未配置见 §4.1 |
| Config | `~/.minicoding/config.toml` 解析成功 | 解析失败检查 TOML 语法 |
| Audit Log | `~/.minicoding/audit.log` 可写 0600 | 权限错误检查目录权限 |

**CI 中集成 doctor**：

```bash
# CI 自检（不连真实服务）
minicoding doctor --security
```

#### 预防措施

- 部署后必跑 `minicoding doctor --security` 验证；
- `doctor` 如实报告沙箱硬化状态（C-22），不虚报 `Hardened: true`；
- `doctor --security` 输出沙箱驱动类型与硬化状态（`docs/dev-plan.md` T-M4-2）；
- 凭证状态检查不泄露 key（只报「已配置/未配置」）；
- `audit.log` 权限 0600，`doctor` 验证可写；
- CI 集成 `doctor` 自检，配置漂移时 CI 失败。

---

### 10.4 会话回放调试

#### 问题描述

生产环境某会话行为异常（如权限决策错、工具调用失败），需在本地复现调试。

#### 解决方案

**导出会话**：

```bash
# 在生产机器导出会话为 md/jsonl
minicoding session export sess_01H... --format jsonl > /tmp/session.jsonl
# 或 md
minicoding session export sess_01H... --format md > /tmp/session.md
```

**回放会话**（本地）：

```bash
# 复现历史工具调用，默认禁用所有副作用工具（C-06）
minicoding --replay sess_01H...

# 如需重放工具，显式允许，且每条仍走权限策略
minicoding --replay sess_01H... --allow-side-effects
```

回放仅重新生成 LLM 响应，不重新执行已记录的工具调用（`docs/security.md` §13.4、`docs/getting-started.md` §4.5）。

**恢复会话**（继续提问）：

```bash
minicoding --resume sess_01H...
# 从 last_compaction_id 定位起始行，继续会话
```

**Fork 会话**（从分叉点尝试不同方向）：

```bash
minicoding --fork-session sess_01H...
```

**OTel trace 关联**：

```bash
# 回放时启用 OTel，对比生产 trace
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
minicoding --replay sess_01H...
# Jaeger 中对比 session_id 的 trace
```

**审计对比**：

```bash
# 生产 audit.log
grep sess_01H... ~/.minicoding/audit.log

# 本地回放 audit.log
grep sess_01H... ~/.minicoding/audit.log
# 对比权限决策差异
```

**注意**：

- `--resume`/`--replay`/`--fork-session` 三者互斥（见 `cli/builder.rs::SessionLoadMode`）；
- 回放默认禁用副作用（C-06），`--allow-side-effects` 显式允许且每条仍走权限；
- 跨进程文件锁（`fs2`）防止两个进程同时写同一会话（`docs/data-model.md` §10）。

#### 预防措施

- `--replay` 默认禁用副作用（C-06），防止回放意外修改文件；
- `--resume` 读 `index.json` 的 `last_compaction_id` 定位起始行，避免全文件扫描；
- `--fork-session` 从分叉点创建新会话，不影响原会话；
- 回放时 OTel trace 与生产对比，定位行为差异；
- `audit.log` 记录决策链，便于对比权限差异；
- 会话 JSONL 前向兼容（`v` 字段，`docs/data-model.md` §2.4），旧会话可回放；
- 敏感会话日志不提交到仓库（`AGENTS.md` §6.4）。

---

## 11. 问题反馈渠道

### 11.1 反馈前自查

提交问题前请完成以下自查，缩短响应周期：

1. **查本文档**：按章节检索同类问题与解法；
2. **查 `docs/getting-started.md` §1.5**：常见构建问题排查表；
3. **跑 `minicoding doctor`**：自检配置与沙箱状态（见 §10.3）；
4. **查 `audit.log`**：权限决策与错误记录（见 §10.1）；
5. **查 OTel trace**：行为异常时看 span 层级（见 §10.2）；
6. **复现**：用 `--replay` 复现会话问题（见 §10.4）。

### 11.2 反馈渠道

| 渠道 | 适用场景 | 格式要求 |
|------|---------|---------|
| GitHub Issues | Bug 报告、功能请求 | 附 `minicoding --version`、`minicoding doctor` 输出、`audit.log` 相关条目 |
| GitHub Discussions | 使用疑问、最佳实践 | 描述场景与预期行为 |
| Pull Request | 代码修复、文档改进 | 遵循 `AGENTS.md` §6 提交规范（Conventional Commits、中文 commit message） |
| Security Advisory | 安全漏洞（凭证泄露、沙箱绕过等） | 私密披露，勿公开 Issue |

### 11.3 Bug 报告模板

```markdown
**环境**：
- minicoding 版本：`minicoding --version` 输出
- 操作系统：`uname -a` / `ver` / `sw_vers`
- Rust 工具链：`rustc --version`（若从源码构建）

**复现步骤**：
1. ...
2. ...

**预期行为**：
...

**实际行为**：
...

**诊断信息**：
- `minicoding doctor` 输出：
- `audit.log` 相关条目（密钥已脱敏）：
- OTel trace 截图（若有）：
- 会话 ID（若可复现）：sess_...

**额外上下文**：
- 配置文件（`~/.minicoding/config.toml`，密钥脱敏）：
- 是否在容器/WSL2 内运行：
- 是否启用沙箱（`--sandbox`/`--preset`）：
```

### 11.4 安全漏洞披露

**不要**在公开 Issue 报告安全漏洞。安全漏洞包括但不限于：

- 凭证泄露（API key 出现在日志/审计/工具结果中）；
- 沙箱绕过（路径越界、命令注入突破 `sandbox_path`/Landlock）；
- 权限绕过（黑名单被覆盖、`AllowAlways` 不该出现时出现）；
- MCP/Hook 越权（project 作用域 server 未弹窗批准、asyncRewake 越权）；
- L0 约束失效（C-01..C-30 任一被绕过）。

披露方式见仓库 `SECURITY.md`（若有）或 GitHub Security Advisory。报告时附：

- 漏洞影响的 L0 约束编号（C-xx）；
- 复现步骤（最小化）；
- 影响评估（凭证泄露/代码执行/权限提升）；
- 建议修复方向。

### 11.5 文档改进

本文档是活文档，发现遗漏或错误时：

- 小修正（错别字、链接失效）：直接 PR；
- 新问题与解法：按「问题描述 → 原因分析 → 解决方案 → 预防措施」四段式补章节，保持编号连续（见 §1.2）；
- 章节重组：先开 Discussion 讨论再 PR；
- 引用准确：用相对路径（`docs/xxx.md`）或 §章节号，不写「见上文」「见下文」（`AGENTS.md` §4.4）。

---

## 参考

- `docs/getting-started.md` §1.5：常见构建问题排查表
- `docs/tech-stack.md` §11：沙箱平台依赖与平台优先级
- `docs/security.md`：威胁模型、权限模型、沙箱边界、审计
- `docs/design.md` §13：性能预算
- `docs/design.md` §17.4：Journal 冲突检测
- `docs/design.md` §18：任务管理（TaskStatus 状态机）
- `docs/rules.md` §2 与 §8：L0 硬约束与约束自检清单
- `docs/hooks.md` §11：asyncRewake 协议
- `docs/data-model.md` §10：跨进程文件锁
- `docs/review-report.md` §4：代码审查发现与建议
- `AGENTS.md` §5：安全规范（开发时）
- `AGENTS.md` §6：提交与协作规范
- `.github/workflows/ci.yml`：CI 门禁配置
- `deny.toml`：依赖许可与 advisory 配置
- `scripts/git-hooks/pre-commit`：pre-commit 钩子
