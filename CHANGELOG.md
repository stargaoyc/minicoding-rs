# Changelog

本文件记录面向使用者的显著变更（BREAKING / 新能力 / 修复）。格式参考
[Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；版本号语义见
`docs/tech-stack.md` §14。

## [0.2.33] - 2026-08-22

### Breaking

- **server 默认强制 API 鉴权**：启动生成 token（stdout `SERVER_TOKEN=`）或
  `--auth-token` 显式指定；脚本需带 `Authorization: Bearer` 或显式 `--no-auth`
  （S1）
- **CORS 默认收敛**为 localhost/127.0.0.1/[::1]；跨域来源需 `--cors-origin`，
  `*` 通配不再支持（S2）
- **last-known-good.toml 不再包含明文 api_key**（保留 `env:` 引用原文；
  S7/C-04）

### Added

- 高危沙箱预设二次确认字段 `confirm_danger`（S3/C-22）
- `GET /metrics` Prometheus 端点 + 进程内指标聚合（P9）
- 存储契约测试框架：内存/JSONL 双后端共享断言，更高格式版本显式拒绝
  （M-13/S-28）
- 前端 Vitest + MSW 单测与 SSE record/replay 快照基建（M-14/W-20）
- 工具输出 render intent 与 `plan.list` 只读工具（M-11/T-15b/T-19）
- 配置热更新白名单（model/turn_timeout_sec/parallel_reads；M-12）与
  `parallel_reads` 并发旋钮
- 会话 step 边界事件持久化与压缩引用链可追溯（M-06/M-07）
- 循环打断软升级：单工具指纹逐级提醒阈值可配（M-08）

### Security

- PreToolUse Hook `modify_input` 后对修改后输入重跑策略检查并取严合并
  （S4/C-01/C-21）
- 内置黑名单扩展至 shell 写约束文件与 `.git/hooks`；预批准缓存词法比对防
  拼接绕过（S5/S6/C-02/C-23）
- shell.run 超时 clamp 至会话上限 + unix 进程组整树终止 + 输出流式字节截断
  （S8-S10/C-07）
- MCP `readOnlyHint` 默认不信任（S13）；`<tool_output>` 边界转义（S21/C-05）
- web.fetch 重定向逐跳 SSRF 复检（S22）；会话/事件/snapshot 落盘 0600
  （S19）；journal 恢复路径组件级包容校验、绝对越界不再绕过（S18 升级）
- `/undo` 落审计 FileUndone（S28/C-28）；Windows 驱动移除 BREAKAWAY_OK、
  is_hardened 如实报告（S24/S25）；Seatbelt profile tempfile 随机名（S26）

### Changed

- 架构治理：builder 组装下沉 sdk（tui 解除对 cli 依赖，A11）；hooks 分发算法
  下沉 minicoding-hooks（A1）；memory→storage 解耦改经 Storage trait（A7）；
  plan_handle/repeat_guard 自 rt.rs 抽取（A6/A4）；全 workspace 依赖方向守卫
  测试矩阵（A8）；路径校验单一实现委托 path_sandbox（S15）；工具分桶
  fail-closed（S14）；fs.write 建目录前包容校验（S16）
- 协议：HTTP DTO 进 ts-rs 导出链 + 自动 barrel 脚本（P1/P2）；JsonValue 绑定
  收敛 generated/bindings（P4）；config_hash wire 类型修正（P5）
- 文档：data-model/design/api/features 等按实现全面对齐（D1-D9）；四份历史
  审查报告标注 superseded（D7）

## [0.3.0] - 2026-08-24

> 本版本为全面审查修复版：基于 `docs/project-review-20260823.md`（基线
> v0.2.33）完成 §3–§12 全部问题与七类遗留项收口，含多项安全默认值收紧。

### Breaking

- **exec 自动化默认 ReadOnly 沙箱**：`minicoding exec` 不再默认继承会话权限，
  写操作需显式 `--auto-approve`；`--i-understand-full-access` 解锁全权访问
  （审查 §9）
- **serve 模式 MCP 写工具默认不暴露**：HTTP/SSE 前端仅见只读工具，写工具需
  `--expose-write-tools` 显式开启（写操作仍逐次走权限门）
- **POST /sessions/{id}/messages 改为 202 Accepted**：turn 异步执行，结果经
  SSE 事件流返回；同步等待响应体的调用方需改订阅 SSE
- **Decision wire 枚举扩展**：新增 `allow_always`/`deny_always`（serde
  snake_case）；前端按钮集与 TUI 键位随之扩展，旧客户端回传两值仍兼容
- **依赖治理**：移除 figment/trait-variant；deny.toml 许可证白名单增补
  MIT-0（jsonschema 传递依赖 borrow-or-share）

### Added

- **OS 网络隔离**：Linux landlock `AccessNet` + macOS Seatbelt `deny network*`
  ——ReadOnly/WorkspaceWrite 档子进程默认禁 TCP/UDP；Windows Job Object 无
  过滤原语，如实标注不限网络
- **AllowAlways/DenyAlways 决策持久化**：`~/.minicoding/policy.toml`
  （原子写、unix 0600），支持 `工具名@路径前缀` 两级粒度、最长前缀优先、
  deny 胜 allow；CLI y/a/n、TUI `a` 键、Web 四按钮四端贯通；C-23 受保护
  文件不查表不落盘防绕过
- **/model 命令**：会话内切换模型（`Runtime::model`/`set_model`），斜杠命令
  Tab 补全；@文件引用补全 + `<file_ref>` 边界注入（32KiB 截断）
- **token 计量贯通**：Usage.output_tokens → MessageMeta.tokens → REPL 逐条
  与会话累计展示；//status //tokens //clear 会话管理命令
- **成本工程**：Anthropic prompt caching（cache_control 断点）、
  thinking_budget_tokens 配置端、calibrate() count_tokens 校准
- **Hook 全量接线**：SessionStart（每会话一次）/UserPromptSubmit 注入派发；
  asyncRewake 后台重派发 + turn 边界 poll 注入 `<async_rewake>` 边界；
  AsyncRewakeScheduler trait + Noop 兜底
- **MCP 加固**：jsonschema 全量入参校验；60s 健康监督自动重启；required
  预检/restart 重试/list_tools annotations/tool_hints 采集；配置加载迁
  minicoding-mcp crate；sdk attach_mcp_tools 完整接线（C-24 批准→启动→注册）
- **Web 前端**：Zod 4.4.3 运行时校验替换手写守卫；Plan 模式可视化面板
  （PlanPanel）
- **工具系统**：新增 ui.ask 工具；grep context/head_limit；edit replace_all；
  shell.kill 真实现（进程组整树终止）
- **CI 门禁**：web job 增 gen-types DTO 一致性校验（生成产物漂移即红）

### Security

- 子进程默认禁网（ReadOnly/WorkspaceWrite 档，配合沙箱双防线 C-22）
- 内置黑名单大小写折叠 + 保护面补齐（AGENTS.md/CLAUDE.md 等写入一律 Ask 且
  不可 AllowAlways）；预批准缓存词法比对防拼接绕过
- SSRF 防护增强：IPv6 mapped/NAT64/CGNAT 地址段拦截
- SAFE_ENV_WHITELIST 单一来源化（修复 serve.rs 全量 env 旁路）；凭证不下传
  子进程 env 强制收敛
- Hook Windows 平台禁占位符展开；Seatbelt profile 转义加固；landlock 放开
  HOME/TMPDIR 读写以对齐 WorkspaceWrite 语义
- MCP serve 只读默认（见 Breaking）+ C-24 project 作用域 server 首次批准强制

### Fixed

- Runtime：权限路径 panic 移除；StopReason 流式透传至 TurnEnd；单 turn 门闩
  （并发 turn 返回 TurnInProgress）；Deny 补发 ToolCallStarted/Finished 事件；
  dispatch 超时兜底
- Provider：Ollama NDJSON UTF-8 边界截断修复；流中错误保文（不再静默吞错）；
  读超时 300s；ApproxTokenizer CJK 计权修正；退避 jitter
- 上下文：tokenizer 计入 ToolResult 文本；repair_request_messages 最后防线；
  post-compact 扫描 tool_calls 字段；熔断 cooldown 半开恢复；L2 摘要改用
  full_text；evict 移除排序副作用；save 并发锁
- 存储/Journal：undo 失败条目回推可重试；HookInput.turn 真实轮次；快照目录
  fsync
- 前端：TUI Ctrl-C 中断执行；TUI 恢复历史渲染与 scrollback 可见区渲染；
  Web 假 Always 收敛为两值；Desktop CSP script-src 收紧；桌面单窗口启动
  （移除控制台窗口，日志写安装目录 logs/）
- 架构：rt.rs 2334→1073 行拆分 permission/denial/sourcing/hot_config/workdir
  五模块；架构守卫覆盖 dev-dependencies；builder 组装接入 config.toml +
  热更新基线
- 文档：security.md 网络矩阵按实现对齐；hooks.md 接线状态诚实化；审查报告
  附录全程跟踪修复状态

[Unreleased]: 后续变更见各 commit（Conventional Commits）。
