# DeepSeek Harness (`dsh`) 项目分析

> 分析对象：`/home/star/deepseek-harness`（WSL Ubuntu-26.04）
> 仓库：github.com/deepseek-ai/deepseek-harness ｜ 分支：master ｜ 版本：`0.1.0-rc.7`
> 分析时间：2026-08-19

---

## 1. 项目定位

DeepSeek Harness（`dsh`）是 **DeepSeek AI 官方的开源 AI 编码智能体框架（agent harness）**，目前处于 **Developer Preview（开发者预览）** 阶段，官方明确提示"未来会有破坏性变更"。

核心设计哲学：**Everything is a plugin（一切皆插件）**，底层由 [Cordis](https://github.com/cordiverse/cordis) 这一依赖注入 / 插件框架驱动（其设计理念来自论文《A Programming Paradigm for Spatiotemporal Composability》）。模型适配器、工具注册表、会话日志、甚至 agent 主循环本身都是可替换的插件。

入口形态：
- `npx @deepseek-ai/dsh web` —— 启动 Web UI（默认 `http://127.0.0.1:3080`）
- 源码运行：`pnpm install && pnpm run build && pnpm dsh web`
- 无头模式：`pnpm dsh --profile headless "修复失败测试"` —— 一次性跑完即退出，适合 CI / 自动化

---

## 2. 技术栈

| 维度 | 选型 |
|------|------|
| 包管理 / 仓库 | pnpm + pnpm workspace（monorepo，7458 个文件含产物） |
| 语言 | TypeScript（host + client 双构建目标）+ Python（SDK）|
| 核心框架 | Cordis（依赖注入 / 插件 / 可组合配置）|
| 前端 | React 18 + Vite（`apps/web` 经 Vite 构建，`dist/` 由 CLI 的 `dsh web` 托管）|
| 测试 | Vitest（单元 / e2e / snapshot / web / perf / stress 多套配置）|
| 质量 | oxlint（Lint）、knip（死代码）、jscpd（重复率）、lefthook（git hooks）|
| 原生 | `native/landlock-run` —— 用 Linux landlock 做进程沙箱 |
| 构建 | tsc（`tsconfig.host.json` / `tsconfig.client.json`）+ tsdown |

---

## 3. 架构核心思想

### 3.1 一切皆插件 + Cordis
没有任何"特权核心"需要打补丁。扩展 `dsh` 的方式就是在其它插件旁边挂一个插件，其注册是"可卸载时自动撤销的 effect"。模型适配器、工具、会话日志、agent 循环全部都是插件。

### 3.2 分层组合：Profile → Bundle → cordis.patch.yml
- **Bundle**：Cordis 配置行 + 挂载代码的发行格式，可被打补丁覆盖。
- **Profile**：命名组合，列出要堆叠的 bundle、用户自带 out-of-tree 插件、以及 `cordis.patch.yml`。
- 基础层 `dsh-base` 提供：模型适配器、工具、持久化、沙箱、审批策略、设置、凭证、遥测。
- 组合顺序：profile 列出的各 bundle → profile 的 `cordis.patch.yml` → home 级 patch → `--patch` 覆盖层。
- 查看实际启动的树：`dsh --profile web --dump-config`。

### 3.3 会话日志是唯一事实来源（Source of Truth）
- `SessionEvent` 是 append-only 日志；`deriveMessages()` 从中投影出模型可见的历史。
- **核心不变量（invariant）**："任何到达模型请求的内容都必须能从日志重建"（Model-visible means logged）。
- 派生能力（fork / resume / 转录 / 遥测 / 持久化）全部基于这条事件流。
- 新增"模型可见输入"必须先扩展 `SessionEventMap` 并从日志渲染 —— 这是强约束。

### 3.4 事件即扩展点（三类域）
- **Session 事件**：持久事实，写入日志并广播（`session/event`）。
- **Agent 事件**（`agent/*`）：携带活跃 `Agent`（inbox / step / status / request / validation），用于观察或拦截在途工作。
- **Capability 事件**：在"接缝"上挂载策略与适配器（`fs/*`、`tools/*`、`telemetry/*`），无需 import 主循环。
- 其中 `agent/pre-step`、`agent/request`、`llm/stream`、三个 `tools/*` 是**瀑布事件**（waterfall，需 `next()` 委派）；`agent/turn-stopping` 是串行无 `next()`。

### 3.5 能力接缝（Seam）
一个可替换能力由三角色构成：服务定义（接口）+ 服务实现（Provider）+ 消费者（通常是面向模型的工具）。
- 文件系统与子进程 Provider 共享同一个"执行世界"：把它们指向远程沙箱，Bash / PTY / LSP 会整体随之迁移，无需分叉 Provider。
- 子代理 Provider 也通过同一接口大幅变化（从全新子 agent 到另一个产品中的委派 turn）。

### 3.6 Turn / Step 流程
- **Step** = 一次模型请求 + 其调用的工具；**Turn** = 零或多步。
- 流程：`turn/start → 认领输入 → 组装 prompt 段 + tool schema → agent/pre-step（可改写/拒绝）→ step/start → llm/stream → tool/call* → step/end →（需要时）下一 step → agent/turn-stopping → turn/end`。
- 输入经由单一 inbox 到达驱动；部分消息立即唤醒，注入上下文等待下条消息。

---

## 4. 仓库结构

```
deepseek-harness/
├── apps/
│   ├── cli/        # @deepseek-ai/dsh：profile 启动、插件管理、web 别名（bin: dsh）
│   └── web/        # @deepseek-ai/dsh-web-frontend：Vite 构建的浏览器壳
├── packages/       # 51 个包（见下）
├── examples/
│   └── headless-agent/   # 无头编码 agent 的 replay + 真实模型测试组合（旗舰示例）
├── python/         # Python SDK：sdk（JSON-RPC 高层 API）+ sdk-runtime（打包运行时）
├── native/
│   └── landlock-run/     # Linux landlock 原生沙箱
├── docs/           # 架构 + 40+ 子系统文档（中英双语）
├── website/        # VitePress 文档站
├── vendor/         # 打包/裁剪的依赖
├── scripts/        # 大量质量/发布/生成校验脚本
└── 根配置：pnpm-workspace.yaml / vitest.* / tsconfig.* / .oxlintrc / knip.json / lefthook.yml
```

### packages/ 51 个包（按职责归类）
- **核心循环**：`core`（session / system-prompt / tools / agent / agent-loop / scope）、`llm`、`compaction`、`context`
- **工具 / 能力**：`shell`、`terminal`、`subprocess`、`fs`、`lsp`、`mcp`、`skill`、`todo`、`goal`、`plan`、`workflow`、`jobs`、`schedule`、`hooks`、`guard`、`approval`（preset）
- **模型 / 执行**：`code-runtime`、`sandbox`、`subagent`、`runtime-diagnostics`、`identity`、`credentials`、`settings`
- **平台 / 接口**：`host`、`client`、`web`、`api`、`acp`、`sdk`、`session-query`、`session`、`storage`、`attachment`、`feedback`、`extensions`、`interaction`、`workspace`
- **支撑**：`boot`、`bundle`、`util`、`typert`（类型运行时）、`spill`、`test-support`、`examples`

---

## 5. 关键子系统（docs/subsystems，40+ 篇中英文档）
session 日志 / system-prompt / tools / agent 循环 / scope / llm 流式 / compaction（压缩）/ filesystem / shell / subprocess / terminal / sandbox / subagent / mcp / credentials / approval / permission-presets / jobs / workflow / goal / plan / settings / storage / persistence / telemetry / session-query / session-projection / session-title / session-reference / user-questions / attachment / workspace / token-meter / invariants / scope / web / web-server / extensions / typert。

---

## 6. 开发与运行

**脚本（节选）**
- `pnpm run build`：构建 host + client 库 + web 前端
- `pnpm test` / `test:e2e` / `test:snapshot` / `test:web*`：单元 / 端到端 / 快照 / 前端多套测试
- `pnpm dsh`：直接跑 CLI（`node --import tsx/esm apps/cli/src/bin.ts`）
- `pnpm demo:code-mode` / `demo:cordis` / `demo:acp` / `mock:llm`：演示与 mock
- `pnpm hygiene`：一条龙静态校验（knip / publint / constraints / 许可证 / 包不变量 / cordis-config / 闭包 / vendor 链接）
- `pnpm check:all` / `check:ci:*`：CI 门禁（linux / static / lint / coverage / snapshot / artifacts / windows / node-compat）

**质量保障特点**：仓库内置数十个 `verify-*` / `gen-*` 脚本（生成并校验 Cordis catalog、config catalog、tool catalog、module graph、mermaid、翻译配对、文档预算等），工程纪律非常严格——这是该项目的显著亮点。

---

## 7. 旗舰能力：headless-agent 示例
`examples/headless-agent` 组合出**完整的无头编码 agent**：DeepSeek V4 + 本地 bash/fs 工具 + 子代理委派 + workflow + 全新 agent 的 Ralph 迭代循环 + `todo_write` + JSONL 持久化。
- 支持 **E2B 沙箱覆盖层 POC**（`e2b.cordis.yml`）：把本地 fs/subprocess 替换为共享 E2B 沙箱，FS/Bash/PTY/LSP 整箱迁移，且沙箱超时/销毁时彻底删除。
- `advanced.cordis.yml` 额外加入 Code Mode 与 Cordis 工具。
- 快照测试通过未导出的 `headless-driver.ts` 把标准会话事件以 JSONL 输出，属于测试设施而非正式 CLI 输出。

---

## 8. 成熟度与观察

**优势 / 亮点**
1. 架构清晰、高度可组合："插件 + 会话日志作为事实来源 + 事件扩展点 + 能力接缝"四件套完整且自洽。
2. 工程化极强：CI 门禁、`verify-*` 自校验、快照测试、中英双语文档、landlock 原生沙箱、Python SDK，质量水位高。
3. 多形态分发：Web UI（消费级）、headless（自动化/CI）、ACP（agent 间协议）、Python 子进程 SDK，覆盖不同集成场景。
4. 沙箱与执行世界统一：fs/subprocess/terminal/lsp 共享同一执行世界，远程化只需替换一层 Provider。

**风险 / 注意点**
1. **仍是 rc 预览版（0.1.0-rc.7）**，官方明示会有破坏性变更，不适合生产关键依赖。
2. 强约束（"模型可见必须可日志重建"、严格的包/配置不变量）带来较高贡献门槛， newcomer 需先吃透 Cordis 与架构文档。
3. 大量 `verify-*` 与 `gen-*` 脚本意味着构建/校验链路较重，首次 `pnpm install` + `build` 成本不低。
4. 模型侧默认对接 **DeepSeek V4**（需 `DEEPSEEK_API_KEY`），生态相对早期，第三方插件数量有限。

---

## 9. 建议的下一步
- 想快速体验：直接 `npx @deepseek-ai/dsh web` 起 Web UI。
- 想做二次开发 / 写插件：先读 `docs/cordis-primer.md` → `docs/architecture.md` → `AGENTS.md`，再研究 `examples/headless-agent` 与 `packages/bundle/base`。
- 想验证环境是否可在本机构建：`pnpm install && pnpm run build`（注意耗时与 Node 版本要求）。
- 想接自己模型 / 工具：注册 `ctx.llm` 适配器或 `ctx.tools` 工具（见架构文档"Where new behavior goes"表）。
