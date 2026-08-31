# 贡献指南

欢迎为 `minicoding-rs` 贡献代码、文档或测试。本文件面向**人类贡献者**；
AI 编码助手请另见 [`AGENTS.md`](AGENTS.md)（项目级 AI 辅助编码约束，含完整
架构/规范/安全约束）。

## 快速开始

```bash
# 构建
cargo build --workspace

# 测试（全量）
cargo test --workspace

# Lint（CI 门禁，必须全绿）
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo deny check        # 依赖许可/安全

# 前端（M9 minicoding-web）
cd crates/minicoding-web && pnpm install && pnpm gen-types && pnpm test
```

## 分支与提交

- **分支命名**：`feature/<crate>-<topic>`（如 `feature/sandbox-landlock-driver`）
- **提交信息**：Conventional Commits，`<type>(<scope>): <subject>`，subject 用中文
  - `type`: `feat` / `fix` / `refactor` / `docs` / `test` / `chore` / `perf`
  - `scope`: crate 名（如 `sandbox` / `policy` / `core` / `tools`）
- **commit 粒度**：一个 PR 一个逻辑变更，不混合多个无关改动

## 编码规范

完整规范见 [`AGENTS.md`](AGENTS.md) §2–§8，核心要点：

- **Rust 2024 edition**，MSRV 1.99+；`edition = "2024"`
- **不 panic**：所有可预期错误走 `Result`；`unwrap()`/`expect()` 仅限测试
- **clippy pedantic**：每个 crate `lib.rs` 顶部 `#![deny(clippy::all, clippy::pedantic)]`
- **路径**：`camino::Utf8PathBuf`（UTF-8 保证）；时间用 `time::OffsetDateTime`
- **异步**：统一 `tokio`；不裸用 `std::thread`
- **错误**：库 crate 用 `thiserror`，边界 crate（cli/sdk）用 `anyhow`
- **公共 API** 必须有 doc comment（`///`），说明用途/参数/返回/错误条件
- **改代码必改文档**（见 [`AGENTS.md`](AGENTS.md) §4.1 对应文档映射表）

## 架构约束

- **依赖方向**：`core` 不依赖任何领域 crate；领域 crate 只依赖 core；
  `tools` 是唯一组合层；禁止循环依赖（CI 有架构守卫测试强制）
- **零实现 core**：`minicoding-core` 只含数据模型、trait、Runtime 编排、
  事件总线、配置、OTel、路径约定、Noop 兜底——**禁止**领域算法
- **trait 定义集中在 core**，实现在领域 crate（见 [`docs/modules.md`](docs/modules.md) §1.4）

## 测试

- 单元测试与源码同文件（`#[cfg(test)] mod tests`）
- 集成测试放 `tests/`，按场景命名
- 异步测试用 `#[tokio::test]`
- **不连真实 OpenAI/Anthropic**：LLM API 测试用 `wiremock`/`httpmock`
- 目标覆盖率 ≥80%（`cargo-llvm-cov`）

## PR Checklist

- [ ] CI 全绿（fmt / clippy / test / audit / deny）
- [ ] 测试覆盖新增逻辑
- [ ] 文档已同步（见 §4.1 映射表）
- [ ] 约束自检清单已过（`docs/rules.md` §8）
- [ ] 无敏感文件提交（`.env` / 凭证 / 会话日志）

## 安全相关

发现安全漏洞请**不要**开公开 issue——按 [`SECURITY.md`](SECURITY.md) 的披露
流程处理（私信 maintainer 或走私下渠道），修复前不公开细节。
