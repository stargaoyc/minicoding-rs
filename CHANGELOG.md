# Changelog

本文件记录面向使用者的显著变更（BREAKING / 新能力 / 修复）。格式参考
[Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；版本号语义见
`docs/tech-stack.md` §14。

## [0.3.3] - 2026-08-25

> 本版本为 `docs/project-review-20260825.md` 全面审查（7 路并行评审）修复版：
> R1–R7 七个阶段收口 3 个 High 安全缺陷与约 20 个 P0/P1 功能缺陷。

### Security

- **policy：shell 黑名单换行绕过封堵**——分段集合与预批准复合拦截补齐
  `\n`/`\r`（`sh -c` 中换行即命令分隔符，此前 `"true\nrm AGENTS.md"`
  完全逃逸 C-02 词法判定）；受保护文件名/VCS 组件剥离尾随点空格
  （Win32 CreateFile 剥离语义导致的 C-23 绕过）
- **core：AllowAlways 粒度收敛**——带路径工具按父目录持久化（`tool@目录`），
  无路径工具仅会话级放行；杜绝"一次按键=跨会话/跨项目全局永久放行"；
  审计 detail 区分持久化/会话来源；policy.toml 路径前缀组件边界匹配
- **sandbox：Windows Job Object 修正**——移除 `BREAKAWAY_OK`（方向相反，
  允许子进程脱离遏制）、JobHandle 保存在驱动内恢复 kill 整 Job 能力、
  分配失败也 resume 防挂起进程泄漏；模块文档去除"受限令牌/CPU 内存上限"
  失实表述；landlock 补 UDP/DNS 残留通道诚实边界注释
- **storage：落盘原子性与权限**——事件流 append 加 SessionLock + 单次
  write_all（消除并发半行损坏）；audit.log/fork 转录/index.json 收紧 0600
- **desktop：auth-token 改经 `MINICODING_AUTH_TOKEN` 环境变量下传**，
  不再出现在 `/proc/<pid>/cmdline`
- **cli：serve 模式 MCP 写工具旗标失效修复**——删除无条件注册调用，
  fail-closed 语义生效

### Fixed

- **tools：worktree 子 Agent 隔离接线**（此前 spec 无 workdir 字段，子 Agent
  在父进程 CWD 工作、隔离整体空转）；merge_back 失败在结果中标注警告
- **core：同批工具调用存在 写→读 依赖时退化为串行保序执行**（此前分桶调度
  会让只读桶先执行读到旧数据）；取消/超时/上限/重复终止的终态提示先落盘
  再广播（此前不入 transcript，resume 后 UI 与历史永久分歧）
- **context：L2 摘要 LLM 调用加 30s 超时**（此前 provider 挂起即持写锁全局
  停摆）；压缩选择按 tool_call/tool_result 配对组原子扩展（消除严格 provider
  400 的孤儿消息根因）；hard_truncate 改 O(N) 单次计 token
- **memory：BM25 snippet 多字节字符截断 panic 修复**；AutoMemory 双文件写
  加串行锁
- **providers：OpenAI o 系/gpt-5 兼容**——`max_completion_tokens` 替代已废弃
  `max_tokens`、省略不支持的 temperature；Anthropic thinking 时 gate top_p；
  流式 `"usage": null` 零值 delta 过滤；SSE 缓冲 16MiB 上限
- **server/web：SSE seq 单一写者**（此前 turn 消费 task 与每个 SSE 订阅各自
  分配 seq，重连出现重复事件）；新增 `POST /sessions/{id}/undo` 与
  `/permission-mode` HTTP 路由（Web/Desktop 补齐回滚与权限模式切换）；
  web event-guard 由 gen-types 管线自动再生成（真实 reasoning_delta 等
  事件不再被静默丢弃）
- **mcp：同名 server 重启前优雅关闭旧连接**；手写 base64 替换为 base64 crate

### Changed

- CI 工具链钉版对齐（pnpm 11.13.0/node 22/checkout v6）；desktop 发布流程
  增加 Release 创建兜底消除并行竞态；移除 insta/assert_cmd 死依赖；
  CHANGELOG 倒序为最新在上
- 文档体系对齐：sandbox-run/seccomp 六处漂移修正、功能统计统一 204、
  security.md 章节号重建、Event 命名以 design §11 为权威、roadmap 追加
  审查遗留延期立项清单（13 项）

## [0.3.2] - 2026-08-24

### Fixed

- **desktop：修复全部 HTTP 请求 `Failed to fetch`**（用户反馈：新建会话
  报"创建失败：Failed to fetch"）。根因：Tauri WebView 的页面 origin
  （Windows `http://tauri.localhost`、macOS/Linux `tauri://localhost`）不在
  server 默认本机 CORS 白名单（S2 精确匹配 `localhost`/`127.0.0.1`/`[::1]`）
  内，preflight 被拒——桌面端所有 HTTP/SSE 请求均受影响，v0.3.1 的"新建会话
  无反应"同为此根因。修复：sidecar 启动参数显式加白两个 WebView origin，
  serve 子命令默认策略不变（外部站点伪造 origin 仍因无 token 而 401）

## [0.3.1] - 2026-08-24

### Fixed

- **desktop：关闭应用即终止 sidecar**（用户反馈：`minicoding-server-sidecar`
  不随应用退出）。根因：窗口 X 关闭被拦截为"隐藏到托盘"应用未真正退出；
  `restart_app` 走 exec 替换进程镜像不触发 `RunEvent::Exit`。修复：关闭窗口
  即退出应用并 kill sidecar；重启前显式 kill；`kill_sidecar` 增加 PID OS 级
  兜底强杀（Windows `taskkill /T /F` / unix `kill -9`）。运行期托盘与
  `Ctrl+Alt+M` 隐藏/恢复保留
- **desktop/web：新建会话失败可见化**（用户反馈：填路径点确认后前端无反应）。
  根因：创建接口失败（典型为 API key 未配置/keyring 不可用返回 500）被前端
  静默吞掉。修复：对话框红色展示服务端错误原文、成功才切换会话
- **server：`POST /sessions` 对 `workdir` 预校验**：目录不存在立即返回 400
  可读报错，不再照常建会话、首个 turn 才在沙箱路径层报错；空白串视为未提供

### Changed

- desktop 发布工具链：Desktop Release workflow 统一钉住
  `nightly-2026-08-18` 编译项目代码（stable 频道 1.98 不满足 MSRV 1.99）、
  tauri-cli 用 runner 预装 stable 安装（外部工具不受项目 MSRV 约束）

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

[Unreleased]: 后续变更见各 commit（Conventional Commits）。
