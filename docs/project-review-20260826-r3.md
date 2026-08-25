# minicoding-rs 第三轮全面审查报告（R3，2026-08-26）

> 审查基线：commit `490e849`（main，工作树干净），v0.3.4。
> 审查方式：八领域并行深审（架构/核心运行时/Provider 与工具/上下文与记忆/安全与沙箱/四形态前端/文档体系/工程化），关键 P0 结论均经人工二次验证（file:line 实读）。
> 本报告取代 `project-review-20260825-r2.md` 成为当前有效审查基线；R2 及更早报告移入 `docs/history/`。
> 修复执行记录见本文 §12（随修复批次追加）。

---

## 0. 总体结论

minicoding-rs 是一个**工程成熟度显著高于同类平均水平**的项目：18 crate 的依赖方向纪律已从文档升级为 CI 强制（12 个 crate 有架构守卫测试）、core 零实现约束基本成立、Noop 兜底哲学贯穿全部 trait、审计/脱敏/fail-closed 等安全习惯扎实、文档量 2.7 万行且章节编号纪律严格、CI 十道门禁 + 三平台矩阵 + 覆盖率硬门槛。两轮历史审查（R1'/R2'）的修复痕迹真实可查。

但本轮审查发现：**多处"上一轮修复"本身引入了新洞**（P0-1 权限缓存半成品、P0-2 提醒注入破坏配对、RT-P1-3 热更新基线方向搞反、P1-6 Anthropic tokenizer 漏同步），暴露出"修复后缺乏对抗性回归验证"的系统性模式。另有 **1 条文档承诺的安全防线完全不存在**（危险命令黑名单）、**1 条 L0 约束缺少决策入口最后防线**（C-23 Always 折叠）、**1 个可直接利用的工具注入面**（git.diff ref）。三处高危全部位于安全关键路径。

**问题统计**：P0×7、P1×22、P2×30+、P3×25+。修复分九阶段执行（见 §11）。

---

## 1. 项目定位与差异化优势

### 1.1 定位评估

对标 Claude Code / Codex CLI，本项目差异化主张基本成立：

| 能力 | Claude Code | minicoding-rs 现状 | 评价 |
|---|---|---|---|
| 多 Provider | 仅 Anthropic | OpenAI/Anthropic/Ollama 三家 | ✔ 差异化成立 |
| 四形态前端 | CLI 为主 | CLI/TUI/Web/Desktop 共享协议层 | ✔ 成立（但有漂移，见 §6） |
| OS 沙箱 | macOS Seatbelt only | landlock/seccomp + Seatbelt + Job Object 三平台 | ✔ 方向领先，强度不均（§5） |
| 会话审计/回放 | 无公开等价物 | JSONL 溯源 + audit.log(0600) + /undo Journal | ✔ 领先 |
| MCP | 一等公民 | rmcp 2.2 接入 + 批准流 | ✔ 基本对齐 |
| Hook 系统 | 有 | 10 类事件 + asyncRewake 协议（executor 未接线） | ◑ 部分 |
| 子代理 | Task 工具成熟 | SDK 通路可用，server/CLI=Noop | ✗ 名实不符（PTM-4） |
| 上下文压缩 | 自动 compact | 4 级瀑布 + 熔断 | ✔ 设计完整（预算口径有洞，CTX-2/3） |
| 记忆 | CLAUDE.md + auto memory | 项目三层 + BM25 检索 | ◑ 检索未接线（CTX-5） |

### 1.2 主要差距（诚实结论）

1. **Anthropic 通路三个账务类缺陷叠加**（usage 覆盖为 0、近似 tokenizer 漏计 tool_calls、thinking 回传缺失致工具循环必挂）——主力场景质量低于宣传；
2. **子代理在 server/CLI 形态不可用**，features.md T-13 却写"已接入生产通路"；
3. **会话摘要与 @memory 检索是"建成未通车"状态**——代码完备但生产路径零调用；
4. Windows 平台隔离强度仅进程级（Job Object），无文件/网络隔离——文档如实但 doctor 引导不足。

---

## 2. 架构审查（18 crate）

### 2.1 做得好的

- **领域互依约束 12/12 CI 强制**，白名单与 manifest 逐一核对零偏差；
- 重依赖物理隔离全部达标（landlock/libseccomp target-cfg 于 sandbox、rmcp 于 mcp、ratatui 于 tui、axum/tower-lsp(opt-in) 于 server、ts-rs 为 optional feature）;
- desktop 的 Tauri feature 隔离堪称范本（默认 `default=[]`，bin required-features，无 webview 可编译全 workspace）；
- prelude 纪律与可见性控制扎实；lint 已收敛 workspace 级（ENG-8）。

### 2.2 问题清单

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| ARCH-1 | P1 | `PolicyPersist`（带磁盘 IO + 匹配语义的具体权限持久化实现）存在于零实现 core，未登记进 modules.md §1.2 模块树，属边界侵蚀起点 | core/src/policy/persist.rs:43-204 |
| ARCH-2 | P2 | 架构守卫结构性盲区：不查 `[target.'cfg(..)'.dependencies]`/`[build-dependencies]` 表；core 守卫漏 dev-deps 而 manifest_guard 查——两套标准不一 | core/tests/architecture.rs:55-59、testing/manifest_guard.rs:18 |
| ARCH-3 | P2 | cli/tui/server/sdk/desktop 五个依赖最复杂的 crate **零架构守卫**（执法不对称） | crates/*/tests 缺失 |
| ARCH-4 | P2 | KEYRING_SERVICE/ACCOUNT 常量复制 4 份（cli/server/desktop/sdk），一处改名即静默 split-brain | cli/cred.rs:31 等 |
| ARCH-5 | P2 | web 组件/App 绕过 hooks 层直调 api 层（违反 AGENTS.md §8.3 分层令）：SetupDialog.tsx:7-14、App.tsx:19,62,151,161 | minicoding-web/src |
| ARCH-6 | P2 | server 直接声明 hyper 全仓零使用（AGENTS.md §7.6 违规） | server/Cargo.toml:54 |
| ARCH-7 | P3 | tools/src/lib.rs 头注描述的"SandboxDriver/Journal 直连装配"已被 trait 注入取代（过时注释） | tools/src/lib.rs:5-20 |
| ARCH-8 | P3 | 根 Cargo.toml 对 reqwest/ts-rs 的注释过期（实际 providers/tools(opt)/core(opt)/server(opt)） | Cargo.toml:82,147 |
| ARCH-9 | P3 | desktop 用 `log` 而非统一 `tracing`，观测面分裂未登记例外 | desktop/Cargo.toml:28 |

---

## 3. 核心运行时（Runtime / Agent 循环 / 事件总线）

### 3.1 做得好的

- 中断处理骨架优秀：三路 select（cancel 优先）+ turn 结束重建 token + backfill 合成结果 + repair 双层防御 + Failed 补发 TurnEnd；
- 锁中毒策略（PoisonError::into_inner）、span instrument 替代 enter（并行路径）、UTF-8 字节缓冲流解析均为正确实践；
- 非测试代码零 unwrap/expect/panic（唯一 unreachable! 有不变式注释）；
- 事件容量治理（1024+16 下限钳制）带回归锁。

### 3.2 问题清单

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| RT-1 | **P0** | **会话级 Allow 缓存早退未做 SEC-1 门控**：`session_allows.contains()` 命中即返回 Allow，发生在 options 门控之前——同工具先获 Always 后，restricted ask（AGENTS.md/auto.md 写入，options 不含 AllowAlways）被静默放行，击穿 C-23/C-27。下方 552 行有正确门控但被本早退短路。审计还把来源记成 "policy allowed"（失真） | core/runtime/permission.rs:501-508 |
| RT-2 | **P0** | **软重复提醒以 System 消息插入 assistant(tool_calls) 与 tool_result 之间**，永久破坏严格 provider 配对：OpenAI 要求 tool 消息紧跟 assistant(tool_calls)，Anthropic 要求 tool_result 在紧随 tool_use 的 user 消息内——两者都会持续 400；且压缩管道永不丢弃 System 消息，污染不可自愈，resume 后仍在。讽刺的是它由防死循环机制触发并把会话推向更大死局 | core/runtime/rt.rs:680-691（对照 622/736 入 ctx 时序）；repair_request_messages 不处理此形态 |
| RT-3 | P1 | **Windows EACCES 常量未平台化**：raw code 13 在 Windows 是 ERROR_INVALID_DATA（数据错误），会被误判为权威沙箱拒绝并计入 C-30 熔断（5 次硬熔断误杀正常负载）。ff48556 只修了 EPERM=5 | core/runtime/denial.rs:39-46 |
| RT-4 | P1 | **seccomp SIGSYS 拒绝无法成为权威判定**：SIGSYS 杀进程表现为退出信号文本→ToolError::Exec(String) 无结构化 errno→只算 advisory 不计熔断。新接入的 seccomp 防线对 C-30 熔断完全失明 | core/runtime/denial.rs:24-32 + sandbox/denial.rs:42-54 |
| RT-5 | P1 | **热更新覆盖保护方向搞反**：baseline 捕获的是含 CLI 覆盖后的最终值（注释宣称相反）；`set_model` 同步 baseline 反而使 `/model` 在下一 turn 边界被 config.toml 文件值打回。"CLI>env>file" 优先级对白名单字段失效 | runtime/builder.rs:460-463、hot_config.rs:91-125、rt.rs:232-244 |
| RT-6 | P2 | repeat_guard 文档口径混乱：结构体文档说硬停止 ≥3，实际取 thresholds.last()=默认 8；streak 按"一轮内出现次数"递增而非"轮次"，一轮双调用提前触发软提醒 | rt.rs:463 vs 645-646,653-659 |
| RT-7 | P2 | 副作用串行路径仍跨 await 持 span.enter() guard（CORE-4 注释自己解释了为什么不行） | rt.rs:1172-1182,1196-1206 |
| RT-8 | P2 | resume 无 snapshot 时 durable_seq=0，SSE durable 恢复退化为全量 RehydrateRequired | runtime/sourcing.rs:51-59 |
| RT-9 | P2 | reload_safe_config 每次 toml 解析两遍（turn 边界热路径双倍开销） | hot_config.rs:64-78 |
| RT-10 | P2 | 只读工具完全绕过 PreToolUse/PostToolUse Hook（含可信 MCP readOnlyHint），审计型 Hook 无法观测只读调用；文档未声明该取舍 | rt.rs:1138-1144 |
| RT-11 | P3 | accumulator 空 id 兜底 unwrap_or_default 产生空串 ToolCallId，多空 id 碰撞 sort/match | accumulator.rs:88 |
| RT-12 | P3 | 取消合成结果文案 "[interrupted] 未执行" 对副作用可能已部分完成的工具语义失真 | rt.rs:943 |
| RT-13 | P3 | switch_workdir 不自带 turn 门闩，靠调用方约定 | workdir.rs:28-29 |

---

## 4. Provider 层与工具系统

### 4.1 功能完整性对照（详见子审查）

Provider 三家横向：SSE/NDJSON 解析健壮性（字节缓冲/16MiB fail-closed）达生产水准；凭证 Debug 脱敏全合规。缺口集中在：

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| PTM-1 | P1 | Anthropic 流式 Usage 被 message_delta 覆盖：input_tokens/cache_read/cache_write 全部落盘为 0（聚合器是替换语义而 delta 只有 output_tokens） | anthropic.rs:456-467 + accumulator.rs:41 |
| PTM-2 | P1 | Anthropic 近似 tokenizer 不计 tool_calls（extract_text 忽略 ToolUse/Image）——tiktoken 侧 §8-P0 同型 bug 未同步，agentic 会话预算系统性低估 → 压缩滞后 → 真实超限 400 | anthropic.rs:601-608 |
| PTM-3 | P1 | Extended thinking + 工具调用组合必然二次请求失败（thinking 块含 signature 未回传，Anthropic 强制要求）——thinking 模式下 agent 循环不可用，且无互斥 gate | provider/trait.rs:66-68、anthropic.rs:274-315,434-438 |
| PTM-4 | P1 | task.spawn 生产通路仅 SDK 接线；server/CLI 落到 NoopSubagentRunner（NotConfigured），features.md T-13 却称已接入生产通路 | server/runtime_builder.rs:314-328 |
| PTM-5 | P1 | web.search 自动跟随重定向且不做逐跳 SSRF 复检/IP pinning（fetch 已修，search 未同步）；甚至未调 validate_url | tools/web/search.rs:86-91 |
| PTM-6 | P0 | **git.diff ref 参数直通 git argv**：option 注入（--output=/home/u/.bashrc 任意写、--no-index /etc/passwd 跨边界读、--ext-diff 外部程序执行）；且 side_effect=None 进只读桶完全免审批，又不接 SandboxDriver——"受控子进程+免审批+无 OS 沙箱"三者叠加的唯一工具 | tools/git/diff.rs:67-84 |
| PTM-7 | P2 | 错误分类缺三类结构化映射：context_length_exceeded/prompt is too long → 裸 Client(400)；401/403 无 AuthInvalid；LlmError::Filtered 定义了但全库零产出（content_filter/refusal 映射 Stopped 静默当正常结束） | openai.rs:356-362,481-488、anthropic.rs:372-378,552-560 |
| PTM-8 | P2 | Ollama 并行工具调用 id 恒空串，回灌换 provider 重放必 400 | ollama.rs:387-390 |
| PTM-9 | P2 | OpenAI 缺 prompt_cache_key；Anthropic messages 侧零 cache 断点（长会话历史每轮全价重算，成本优化只完成 system/tools 前缀一半） | openai.rs:112-170、anthropic.rs:128-158 |
| PTM-10 | P2 | Ollama 缺 keep_alive（5 分钟卸载冷启动）；num_ctx 硬编码 8192 与模型无关 | ollama.rs:140-168,185-196 |
| PTM-11 | P2 | WorktreeSubagentRunner git 子进程继承完整环境变量（凭证泄入 repo hooks，C-04 旁路；对比 git/diff.rs 都做了 env_clear） | tools/worktree.rs:57-190 |
| PTM-12 | P2 | ui.ask 权限问答不落 audit.log（§5.5 违规） | tools/ui.rs:101-134 |
| PTM-13 | P2 | shell.run 超时丢弃已捕获部分输出；非零退出码 is_error=false | tools/shell/run.rs:197-213,239-253 |
| PTM-14 | P2 | Anthropic 非 thinking 路径 max_tokens clamp 8192 过时（现模型 32k-64k+，静默压低用户配置） | anthropic.rs:509-534 |
| PTM-15 | P3 | fs.read 无行号输出（CC Read parity 差异，edit 定位费 token）；fs.multiedit 不支持 per-edit replace_all；fs.glob 无条目数上限；MAX_OUTPUT_CHARS=10000 硬编码偏小；二进制文件探测缺失 |
| PTM-16 | P3 | SSE data payload trim 过度；stream_options.include_usage 无条件发送（旧 vLLM 400）；reasoning_effort 未下发（trait 声称映射但实现忽略）；lib.rs 头注声称 Azure OpenAI 支持但无 deployment 形态；Retry-After 仅秒制 |
| PTM-17 | P3 | fs.write mkdir 分支 TOCTOU 微窗口（landlock 启用时内核兜底）；后台 shell 缓冲区存原文未脱敏；worktree merge 成功分支强删分支湮灭未提交改动；Ollama 多行 tool_calls index 各自从 0 起互相覆写 |

---

## 5. 安全权限模型与三平台沙箱

### 5.1 决策流水线验证结论

黑名单→Plan 门→预批准→Hook 取严合并→Prompter→单点审计的主链实现正确：C-02/C-21 顺序有回归锁、Hook allow 无法覆盖黑名单 Deny（registry 预置 Deny 双闸）、四种 Prompter 审计全覆盖、replay 默认禁副作用（全变体参数化测试）、redact 前 4 字符+*** 达标、DNS pinning + 逐跳复检（web.fetch）业界良好水平。

### 5.2 问题清单

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| SEC-1 | **P0** | **危险命令正则黑名单是幻影约束**：security.md §4.2 列出 rm -rf /、fork bomb、mkfs、dd of=/dev/、curl\|sh、chmod -R 777 / 六类并断言 Deny 不进 Ask；rules.md C-02 同样承诺。实际 is_blacklisted 仅覆盖约束文件删除/VCS 元数据写入两类。auto-approve/full-access 场景下注入命令零阻力 | docs/security.md:219-235,89,125 vs policy/builtin.rs:208-224 |
| SEC-2 | **P0** | **C-22 二次确认存在直通旁路**：confirm_danger 门控只挂在 preset 上；`POST /sessions` 直接携带 `permission_mode: bypass_permissions` 或运行时 `POST /permission-mode` 切换均无确认、无 audit（仅 info 日志）。BypassPermissions 下所有副作用免弹窗自动放行，而 web.fetch 在主进程内不受沙箱约束 | server/http.rs:564,873-883 |
| SEC-3 | **P0** | **决策入口缺少 C-23 最后防线**：prompter 返回 AllowAlways/DenyAlways 时无条件 persist_and_collapse_always，不校验 prompt.options 是否提供过 AllowAlways。Web 前端 PermissionDialog 因协议 DTO 无 options 字段恒渲染四个按钮——一次误点"始终允许"即把 fs.write@项目根 写入 policy.toml（目录级永久免审批提升链）。TUI 已正确降级（证明前端自觉不可靠，必须在 core 决策入口强制） | core/runtime/permission.rs:593-615 + protocol/event.rs:64-70 + web PermissionDialog.tsx |
| SEC-4 | P1 | contains_directive 仅行首前缀匹配：`- Never ...`（Markdown 列表，最常见形态）/`1. Always...`/`*必须*`/`### Rules`/敬语前缀全部 MISS——auto.md 构成跨会话全局持久注入通道（C-27 形同虚设）；反向"应用服务器"误报训练用户机械点确认 | core/util/mod.rs:32-68 |
| SEC-5 | P1 | 预批准复合操作符清单缺重定向符 > >> < <> &> >|：plan.exit 批准 cargo build 后 `cargo build > ~/.ssh/authorized_keys` 词法命中免弹窗（Windows Job Object 无 FS 隔离，真实发生） | policy/builtin.rs:137-144 |
| SEC-6 | P1 | macOS Seatbelt `(allow file-read*)` 全盘放行：~/.aws/.ssh 凭证在沙箱内完全可读（Linux A3 白名单收敛后形成平台断层）；`(allow signal)` 无目标限定 | sandbox/macos.rs:197,200 |
| SEC-7 | P1 | Linux "默认禁网"实为仅禁 TCP（UDP/DNS 外泄通道开放，seccomp 默认关；ABI<4 连 TCP 都不禁；Windows 无任何网络过滤）——security.md §1.3 安全论证在三平台分别打 6 折/0 折/0 折 | linux.rs:260-266,241-247 |
| SEC-8 | P1 | seccomp 仅 Native arch：x86_64 上 i386 兼容 syscall 全面旁路 deny-list（int 0x80）；缺 clone namespace 标志位过滤 | seccomp.rs:104-106 |
| SEC-9 | P1 | policy/ssrf.rs 是弱化版平行实现（无 IPv4-mapped/NAT64 解包/pinning）且全库零调用；security.md 声称 MCP HTTP transport 会调 SSRF 校验——实际 grep 零命中 | policy/ssrf.rs:164-181 |
| SEC-10 | P1 | shell 词法近似的现实绕过样本：sed -i.bak / --in-place 特判只认精确 `-i`；dd of=AGENTS.md 参数式目标 file_name 含 of= 前缀比对失败；V=AGENTS.md; rm $V 变量展开缺失 | builtin.rs:300-318,306 |
| SEC-11 | P2 | 沙箱拒绝（authoritative 熔断）不入 audit.log——最值得取证的事件无痕 | denial.rs:164-272 |
| SEC-12 | P2 | Landlock ABI<V3 时 truncate(2) 不受约束（SEC-2 warn 只提网络）；ioctl_dev(v5) 全版本未提 | linux.rs:122-137,249-251 |
| SEC-13 | P2 | Unix 缺 parent-death 机制：主进程 OOM-kill 后孤儿任务树继续运行（Windows KILL_ON_JOB_CLOSE 天然覆盖） | shell/run.rs:125-136 |
| SEC-14 | P2 | Windows apply/post_spawn FIFO 错配残余（A 拿 B 策略）；EACCES 死分支误导维护者 | windows.rs:37-54、denial.rs:45-46 |
| SEC-15 | P2 | external-sandbox + auto-approve 组合等价 root 化，CLI 帮助/启动时无 red 警示 | external.rs:38-55 |
| SEC-16 | P2 | denial 签名表三平台不一致：macOS "sandbox-exec"/"Sandbox violation" 两签名在本项目 sandbox_init(3) 用法下不可达 | sandbox/denial.rs:56-67 |
| SEC-17 | P2 | 两套 redact 行为漂移：policy/redact.rs 末赋值吞尾未同步 PTM-14 精确边界；关键词/前缀表各自维护 | policy/redact.rs:120-147 vs shell/run.rs:343-433 |
| SEC-18 | P3 | mode.rs Preset 生产死代码与 server build_preset_policy 平行实现漂移（FullAccess→Never vs →BypassPermissions）；security.md §2.2 "fs.read 越界→Ask"未实现（实际硬拒，方向偏严）；会话级缓存命中审计溯源失真；macOS .sb profile fork 前失败残留；hardening LD_ 不含 DYLD_（cfg(linux) 无实害） |
| SEC-19 | P3 | SSRF 黑名单小缺口：0.0.0.0/8 整段、240.0.0.0/4、192.0.0.0/24、64:ff9b:1::/96 未拦 | tools/web/ssrf.rs:105-133 |

---

## 6. 四形态前端一致性

### 6.1 结构性根因

**Runtime 双轨组装**：CLI/TUI 走 `sdk::builder`（记忆注入/Hook/memory.write/git/web 工具/PolicyPersist/ConfigWatcher/task.spawn 全齐），server 走独立 `runtime_builder.rs`（以上能力系统性缺席，system prompt 为纯默认句）。这是下表大部分漂移的单一根因。

### 6.2 问题清单

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| FE-1 | P1 | SSE durable recovery 是死代码：cursor evict 且 ≤durable_seq 时返回 Some(vec![]) 提前 return，EventStore::load_after 任何输入都不可达；server 重启后 Last-Event-ID ≤persisted 重连得到空重放+直接转实时流，断线事件静默丢失 | session_mgr.rs:158-192、protocol/cursor.rs:88-108 |
| FE-2 | P1 | turn Failed 不发 TurnEnd（HTTP 202 异步形态）：provider 报错后 Web isStreaming 永久 true、输入框禁死只能刷新页面 | cli/interactive.rs:38-41 自证、http.rs:718-747、web chatReducer.ts:94-115 |
| FE-3 | P1 | RehydrateRequired 固定 id:0 + EventSource 自动重连：全量历史重放顶掉现行权限弹窗（僵尸弹窗）、cursor 空时无限重连风暴 | sse.rs:62-67,163-166 |
| FE-4 | P1 | NDJSON/ACP Lagged 仅 tracing.warn（E-14 未落地且注释相反）；NDJSON Undo 硬编码"不支持"而同一进程 HTTP 已完整实现（journal 已注入）——同进程行为分裂 | ndjson.rs:337-350,437-441、acp.rs:578-580 |
| FE-5 | P1 | Desktop keyring 失败直接启动错误屏，无文件 fallback（CLI 有 credentials 0600 降级）——headless Linux 用户 Desktop 不可用而 CLI 可用 | desktop/config.rs:91-100 vs sdk/cred.rs:23-43 |
| FE-6 | P1 | LSP "Allow Always" 静默折叠为一次性 Allow（依据过时注释）；$/cancelRequest 承诺未兑现但 WorkDoneProgress 宣告 cancellable=true | lsp.rs:418-428,470-475 |
| FE-7 | P2 | ServerPrompter pending 表泄漏：turn cancel 后 entry 残留，Web 5s 轮询不断复活幽灵权限弹窗 | prompter.rs:99-144 |
| FE-8 | P2 | 会话表无 TTL/容量治理：HashMap 只增不减，每会话常驻 2 task+broadcast+ring buffer，内存单调上涨 | session_mgr.rs:196-208 |
| FE-9 | P2 | workspace_read 整文件读入后才截断（数 GB 日志 OOM 面，C-07 失效） | workspace.rs:174-179 |
| FE-10 | P2 | TUI 回看偏移双重作用（切片+.scroll 叠加）；吸底时每 Token 帧全量 Markdown 重解析 O(buffer²) | tui/view/chat.rs:31-36,118-127 |
| FE-11 | P2 | serve stdio 三分支硬编码安全配置忽略 --preset（IDE 无法选 read-only） | cli/commands/serve.rs:454-575 |
| FE-12 | P2 | sidecar 崩溃无检测/重启/通知；W-07 updater 声称已实现但 tauri.conf 无 updater 段 | sidecar.rs:307-321、features.md W-07 |
| FE-13 | P2 | 端口发现靠 rfind(':') 解析 tracing 日志文本（ANSI 色码即可破坏）；stdout fmt layer 未关 ANSI | sidecar.rs:116-131、otel_init.rs:180 |
| FE-14 | P2 | ACP 方法名与官方规范不符（newConversation vs session/new 等），Zed 直连大概率 method_not_found；features.md E-12 称可嵌入 Zed | acp.rs 全文 |
| FE-15 | P2 | ?token= 查询参数对所有端点生效（泄漏面大于必要）；HTTP 错误响应三种形状（envelope/axum rejection/裸字符串）；design.md §24 端点清单 8 个 vs 实际路由 14 个 | http.rs:450-463,291-306 |
| FE-16 | P3 | 单次/exec 尾部 token 截断竞态（无 RENDER_FLUSH_TIMEOUT）；ReasoningDelta 形态内漂移（REPL 渲染/单次 exec 丢弃）；TUI 缺 --plan/--resume/--replay flags、/quit 未知；TUI 权限弹窗固定 60×7 无滚动、DenyOnce/DenyAlways 无键位；lsp.rs 伪测试无断言；GET 带 Content-Type 触发多余 preflight；list_sessions 锁内线性扫描；serve workdir 未 canonicalize；i18n 混用（LSP 英文其余中文）；client.ts 生产 console.debug；jsonrpc Response 双 None 可反序列化；tauri CSP connect-src 本机任意端口 |

---

## 7. 上下文管理与记忆系统

### 7.1 机制核实

4 级压缩 = L1 工具结果裁剪(>2000 chars)→L2 LLM 摘要(最低权重 50%)→L3 滚动窗口(留 20 条)→L4 硬截断，配对组原子操作；熔断是包裹管道的状态机（Runtime 持有，LLM 不可触达，C-29 达标）+ Thrash 双计数。记忆三层 = project_doc(@import≤3 层)/long_term/auto 物理隔离达标；检索为纯本地 BM25+CJK 逐字分词（零网络依赖，诚实）。

### 7.2 问题清单

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| CTX-1 | P0 | （同 SEC-4，归口安全阶段）contains_directive 绕过使 auto.md 成全局持久注入通道 | core/util/mod.rs:32-68 |
| CTX-2 | P1 | 预算口径漏 system prompt 与 tool schemas（project_doc 可达 8K token 未计入）；context_window 取自 small provider 而非主 provider（配置反差时预算虚高→真实超窗→熔断锁死） | manager.rs:301、sdk/builder.rs:343-350 |
| CTX-3 | P1 | calibrate() 口径混搭：messages-only 缓存与 full-request 计费值做 midpoint（系统性高估烙印固定开销）；Usage::Default 零值腰斩缓存（低估→该压不压） | manager.rs:450-460 |
| CTX-4 | P1 | 会话摘要在生产路径零调用方（CLI/TUI/server 均未接线），index.json.summary 恒 None——特性端到端失效 | rt.rs:414-452 全仓 grep |
| CTX-5 | P1 | @memory BM25 检索契约生产端零写入（query_slot 无调用方），超 4096 字符尾部条目静默截断无日志 | auto_contributor.rs:102-114、sdk/lib.rs:123,143 |
| CTX-6 | P2 | L1/L3 参数与权重模型全硬编码（clip 2000/rolling 20/base 0.9-0.4）；sticky 恒 false（M5 TODO）——错误工具结果与普通结果同权可被摘要掉 | compress/mod.rs:109,135-141、weight.rs:24-49 |
| CTX-7 | P2 | reserved_output=4096 是纸面数字（请求 max_output_tokens=None，实际由模型默认决定） | budget.rs:17 vs manager.rs:589-597 |
| CTX-8 | P2 | 熔断后恢复路径近乎不存在：ContextManager 无 truncate API，/clear 仅清屏，BudgetExceeded 无自救指引 | circuit_breaker.rs、cli/interactive.rs:137-142 |
| CTX-9 | P2 | L2 摘要 200 token 上限过小且 prompt 无结构化保留清单（用户早期指令恰是权重最低最先被摘要对象） | summarize.rs:25,120 |
| CTX-10 | P2 | 记忆文件 0644 落盘（audit/persist 均 0600，唯独记忆正文没走 fs_private） | long_term.rs:174-177、auto.rs:245-256 |
| CTX-11 | P2 | Auto memory 全局单库跨项目污染（项目 A 偏好注入项目 B，与 AGENTS.md 冲突且优先级不明）；memory.write long_term 盲批全量覆盖+AllowAlways | sdk/builder.rs:270、builtin.rs:181-186 |
| CTX-12 | P2 | 压缩对前端零事件零提示（Event 无 Compress 变体，模型突然失忆无从归因） | event.rs:12-73 |
| CTX-13 | P2 | Image 计 0 token（vision 大户低估）；ToolContent::Json 不裁但计价（500KB JSON 直接推进 L3/L4 丢历史） | model/message.rs:192-216、clip.rs:68 |
| CTX-14 | P2 | post_compact 每次压缩后膨胀 system 且破坏 prompt cache；默认 50K 预算占 128K 窗口 39%；只匹配字面 "fs.read" | manager.rs:553-579、post_compact.rs:32-60 |
| CTX-15 | P3 | inject.rs 双函数死代码（<long_term_memory> 边界生产走 <user_rules> contributor，文档幻影）；SimpleContextManager 降级时压缩/预算/熔断全消失仅一行 warn；weight recency 分母含 system；去重 topic 精确匹配无 TTL |

---

## 8. 文档体系

### 8.1 核实通过的项

features.md 统计表 205 项精确一致；rules.md C-01..35 编号连续；design/modules/security/api 章节连续；事件清单经 DOC-1 清理后与代码逐字一致；端点清单 api.md §9 与 http.rs 路由一致；data-model JSONL 格式高度一致（D1 重写质量高）。

### 8.2 问题清单

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| DOC-1 | P1 | 幻影安全约束：危险命令黑名单（§4.2 八条正则）+ 敏感路径写 Deny（.env/*.secret）均无实现（同 SEC-1） | security.md:219-235,89 |
| DOC-2 | P1 | audit.log 示例字段失实（turn/input/rule/ok/reason vs 实际六字段 AuditRecord） | security.md:333-337 |
| DOC-3 | P1 | `minicoding auth login` 命令不存在（实际 cred store） | getting-started.md:489-501 |
| DOC-4 | P1 | README `minicoding --tui` 不存在（独立二进制 minicoding-tui） | README.md:178 |
| DOC-5 | P1 | AsyncRewakeSpec 三份文档描述不存在的字段（task_id/wake_prompt/Duration vs 实际 estimated_duration_sec/description/u32）；接线状态三种说法互相矛盾（hooks.md 说未接线 / rt.rs 已派发生命周期 / permission.rs 注释说后续任务——实际 rewake spawn 无触发路径） | hooks.md:307-330、api.md:905-908、rules.md C-32 |
| DOC-6 | P1 | design.md §8.6 AGENTS.md 加载算法 4 处失实：override 层未实现、fallback 配置键不存在、@import 前缀与深度错（@import /3 vs @/5） | design.md:1069-1101 |
| DOC-7 | P2 | api.md LlmProvider 四处签名漂移（chat 默认方法/Delta::Reasoning/thinking_budget_tokens/SystemPrompt）；ToolContext 缺 max_read_bytes | api.md §3.1,§3.3 |
| DOC-8 | P2 | hooks.md 幽灵事件名 ToolCallStart/End；seccomp 状态四处陈旧（AGENTS/tech-stack/getting-started/roadmap 仍写"待接入"，security.md 已记载落地） | hooks.md:50 等 |
| DOC-9 | P2 | mcp_choices.toml 结构漂移（扁平 choices vs fingerprint 分桶嵌套）；development-process.md §3.4 仍论证已被推翻的 sandbox-run 决策（无反转注记）；getting-started parent_uuid/learning-guide/troubleshooting 引用已删 JSONL 设计 | data-model.md:370-380 等 |
| DOC-10 | P2 | features.md 工作量表 M8=6 vs dev-plan M8=9（合计 71 vs 74 人日）；"≈204"vs 205 | features.md:360-371,326 |
| DOC-11 | P2 | 交叉引用失效 4 处（modules.md §15.7/design §1.3/api §24/data-model §2.4） | observability.md:31 等 |
| DOC-12 | P3 | modules.md §1.2 core 模块树漂移（worktree.rs 仍在 core 树、缺 persist.rs/util 文件、prelude 示例与实际导出严重不符）；README "204 项"、cargo-dist.toml 旧名、Vite 6、design "24.x" 占位编号；11 份过程文档约 190KB 与权威层混居（建议 docs/history/ 归档，保留 r2 为基线） |

---

## 9. 工程化质量（CI/CD/测试/版本）

### 9.1 现状评价

十道 CI 门禁、三平台矩阵+交叉编译前置拦截、llvm-cov ≥80% 硬门禁、39 tag 发布史、pre-commit/pre-push 双轨。整体远超同体量平均。

### 9.2 问题清单

| 编号 | 严重度 | 问题 | 位置 |
|---|---|---|---|
| ENG-1 | P1 | 发布产物零签名、attestation 显式关闭、desktop 产物连 checksum 都没有——安全敏感项目分发无法验证完整性的二进制 | dist-workspace.toml:18、desktop-release.yml:148-158 |
| ENG-2 | P1 | MSRV 1.99 不可验证（全部 job 硬编码 nightly-2026-08-18，stable 1.98 无法编译）——虚假声明 | Cargo.toml:35、desktop-release.yml:59-61 |
| ENG-3 | P1 | cargo test 污染被跟踪源码（ts-rs 导出副作用），已两次事故提交还原 | HEAD 490e849、70a4e06 |
| ENG-4 | P1 | TUI 完全没有安装 tracing subscriber——观测性四入口缺一，所有 tracing 事件静默丢弃 | tui/main.rs 全文 |
| ENG-5 | P2 | metrics 语义 bug：gauge 当累加 counter（set_active_sessions(5),(3) 得 8 且标 # TYPE counter）；record_mcp_tool_call 不进 /metrics；两个"直方图"无聚合实现 | core/metrics.rs:27-31,181-189,245-252 |
| ENG-6 | P2 | server 日志轮转注释承诺"保留 7 份"但 RollingFileAppender::new 无清理（磁盘无界增长）；server 默认 filter info 与 CLI warn 不一致但注释声称对齐 | server/otel_init.rs:66,186,44 |
| ENG-7 | P2 | 无 dependabot/renovate；reqwest 0.12+0.13 双版本同时链接等 13 组重复依赖；deny multiple-versions=warn 仅告警 | .github/ 缺失 |
| ENG-8 | P2 | 版本管理松散：cliff.toml 是僵尸配置（CHANGELOG 从未由其生成）；tag 缺 v0.2.32 跳号无解释；20 天 33 个 patch tag；CHANGELOG 日期与 tag 错位 | cliff.toml、CHANGELOG.md |
| ENG-9 | P2 | 本地钩子双轨漂移（scripts/git-hooks 与 pre-commit-config 的 typos/secrets/whitespace 规则各不相同） | .pre-commit-config.yaml |
| ENG-10 | P2 | 覆盖率豁免结构性（cli/tui/server 三入口排除）+ visibility 步骤 \|\| true 恒成功不上传；web 无 Playwright E2E（AGENTS §8.8 要求）；journal/mcp 等 9 crate 集成测试仅守卫 | ci.yml:106-121 |
| ENG-11 | P2 | GitHub Actions 浮动 tag 引用（dtolnay/rust-toolchain@master 尤其危险）；windows-target-check 仅覆盖 6/18 crate（pre-push 反而更多——本地比 CI 严是倒挂） | ci.yml |
| ENG-12 | P3 | 仓库根 __pycache__/tmp 个人杂物（ignore 生效中但习惯危险）；.gitignore dist/data/tmp 未锚定；deploy/ 命名易误解（实为 OTel 观测栈 compose）；_typos.toml 恒等映射冗余且 exclude 全部 docs；workflow 五处硬编码工具链日期需改 6 处；tauri-cli "^2" 主版本浮动曾致发布失败 |

---

## 10. 可靠性与风险综合评估

### 10.1 生产就绪度评分（5 分制）

| 维度 | 得分 | 说明 |
|---|---|---|
| 架构健康度 | 4.5 | 守卫强制 + 边界清晰，扣分项：Persist 未登记、守卫盲区 |
| 安全防线深度 | 3.0 | 主链正确但幻影防线/旁路口/沙箱强度不均拉低 |
| 正确性可靠性 | 3.0 | RT-2 配对污染、PTM-1/2/3 三家账务缺陷、FE-2 前端卡死 |
| 功能完整性 | 3.5 | 内置工具集齐但子代理/摘要/检索"建成未通车" |
| 一致性 | 3.0 | 双轨 builder 导致 server 形态能力系统性缩水 |
| 文档可信度 | 3.5 | 权威层好，用户层/安全矩阵有失实 |
| 工程化 | 4.0 | CI 强，供应链末端与版本节奏弱 |

**结论**：当前状态适合**个人/小团队 Linux/macOS 默认预设下使用**；作为"生产级"尚需完成：P0 全部 + 沙箱网络面如实披露或补强 + server 形态能力对齐 + 发布链签名。

### 10.2 Top 风险（攻击者视角）

1. `git.diff {"ref":"--output=~/.ssh/authorized_keys"}`：免审批任意写（PTM-6）
2. HTTP `permission_mode: bypass_permissions` 直通：免弹窗全自动 + web.fetch 主进程外联（SEC-2）
3. auto.md 列表格式投毒：一次免审批写入 → 所有未来会话 system 注入（CTX-1）
4. Web"始终允许"按钮 → 目录级永久免审批提升链（SEC-3）
5. plan.exit 预批准 + 重定向符 → Windows 上真实越权写（SEC-5）

---

## 11. 修复计划（九阶段）

| 阶段 | 范围 | 关键项 |
|---|---|---|
| R3-1 | 权限链 P0 | RT-1 门控、SEC-3 决策入口折叠、SEC-2 http 确认、PTM-6 git.diff、SEC-4/CTX-1 contains_directive、SEC-5 重定向符、PTM-11 worktree env、PTM-12 ui.ask 审计 |
| R3-2 | 运行时 | RT-2 配对修复、RT-3 EACCES、RT-5 热更新基线、RT-6 口径、RT-7 instrument、RT-8 durable_seq、RT-9 双解析 |
| R3-3 | Provider | PTM-1 usage 合并、PTM-2 tokenizer、PTM-3 thinking gate、PTM-5 search 重定向、PTM-7 错误分类、PTM-8 Ollama id、PTM-9 cache、PTM-10 keep_alive、PTM-14 clamp |
| R3-4 | Server/Frontend | FE-1..6 + FE-7..15 择要 |
| R3-5 | Context/Memory | CTX-2..14 择要 |
| R3-6 | Sandbox/Policy | SEC-1 黑名单落地、SEC-7/8 如实披露+arch 补齐、SEC-11 审计、SEC-10 词法补强、SEC-17 redact 统一、SEC-19 ssrf 补段 |
| R3-7 | 架构守卫 | ARCH-1..6 |
| R3-8 | 工程化 | ENG-1..11 |
| R3-9 | 文档 | DOC-1..12 + history 归档 |

## 12. 修复执行记录

（随批次追加）
