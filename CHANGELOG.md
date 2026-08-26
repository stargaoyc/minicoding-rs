# Changelog

本文件记录面向使用者的显著变更（BREAKING / 新能力 / 修复）。格式参考
[Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；版本号语义见
`docs/tech-stack.md` §14。

## [0.3.5] - 2026-08-26

> 本版本为 `docs/project-review-20260826-r3.md` 第三轮全面审查修复版：
> 八领域深审收口 P0×7/P1×22 及多项 P2/P3，分九阶段落地。核心主题是清除
> "上一轮修复引入的新洞"与一条幻影安全防线，关键回归测试均经变异验证。

### Security

- **policy：危险命令词法黑名单落地**——security.md §4.2 此前承诺的防线
  无实现（幻影约束）：fork bomb/mkfs/dd of=/dev/rm 递删根/chmod -R 根/
  curl|wget 管道执行远程脚本六类现硬 Deny 且不可覆盖（C-02）；sed 原地写
  变体（`-i.bak`/`--in-place`）与 `dd of=` 参数式写约束文件补拦；预批准
  复合操作符补齐重定向族（`cargo build > ~/.ssh/authorized_keys` 不再借
  词边界前缀免弹窗）
- **core：权限决策入口最后防线**——前端回传的 AllowAlways/DenyAlways 在
  prompt 未提供 Always 选项时一律折叠为一次性语义且不落缓存/持久化
  （Web 曾恒渲染"始终允许"按钮，一次误点即目录级永久免审批提升链）；
  会话级 Allow 缓存早退删除——restricted ask（AGENTS.md/auto.md 写入）
  不再被同工具此前的 Always 静默放行（RT-1 击穿 C-23/C-27 通道）
- **server：BypassPermissions 直通口封堵**——创建会话直携
  `permission_mode: bypass_permissions` 与运行时 `/permission-mode` 切换
  现均要求 `confirm_danger: true`（C-22），权限模式变更落审计
- **tools：git.diff ref 安全校验**——拒绝选项形态与 ref 非法字符，封堵
  `--output=<file>` 沙箱外任意写与 `--no-index` 跨边界读注入面（该工具为
  只读桶免审批 + 不接 OS 沙箱，三层防线曾全部旁路）；worktree 子代理 git
  子进程环境白名单化（凭证泄入仓库钩子通道关闭，C-04）；ui.ask 决策落
  audit.log（PTM-12）
- **core：auto.md 指令性内容检测重写**——剥离 Markdown 修饰前缀后词级匹配，
  列表/加粗/多级标题旁路封堵（`- Never commit secrets` 此前免审批写入全局
  auto.md 并注入所有未来会话）；降级 Ask 改用不含 AllowAlways 的选项集；
  中文单字 `应` 收紧为双字组合消除高频误报（CTX-1/SEC-4）
- **sandbox/seccomp**：x86_64 补 X86/X32 兼容架构段（堵 `int 0x80` 旁路
  deny-list）；沙箱拒绝（authoritative/advisory + 熔断计数）落 audit.log；
  Windows EACCES(13)=ERROR_INVALID_DATA 平台化修正（不再误计入 C-30 熔断）；
  macOS denial 签名修正为 sandbox_init 真实失败文案；SSRF 补拦 0.0.0.0/8、
  240.0.0.0/4、192.0.0.0/24、local-use NAT64

### Fixed

- **core：软重复提醒破坏 provider 配对（P0）**——System 提醒此前直插
  `assistant(tool_calls)` 与 tool_result 之间，OpenAI/Anthropic 自触发点起
  对本会话持续 400 且压缩管道永不清理、resume 后仍在；改走 hook_context 同款
  缓冲并入下一请求 system 头部（新增配对不变式回归测试）
- **providers：Anthropic 三处账务/循环缺陷**——流式 Usage 被 message_delta
  整包替换致 input/cache 计量全为零（改合并语义）；近似 tokenizer 漏计
  tool_calls JSON 致压缩滞后真实超窗（改用 full_text）；extended thinking +
  工具调用组合显式报错（thinking 块未回传时第二跳必 400，神秘失败转明确
  提示）；Ollama 并行工具调用合成稳定 id（原空串碰撞）；OpenAI 下发
  prompt_cache_key、Anthropic messages 侧第三缓存断点；Anthropic max_tokens
  clamp 8192→32768；401/403 与上下文超长结构化为 AuthInvalid/ContextLength；
  web.search 禁用自动重定向对齐 fetch 的 SSRF 防线
- **core/runtime：热更新覆盖保护重构**——基线比对方向搞反使 CLI 覆盖被
  config.toml 回退、`/model` 选择只存活一轮（改为显式覆盖集合：
  builder 登记 CLI/env 覆盖，set_model 登记运行期切换）；repeat_guard 硬停止
  口径统一为末级阈值（默认 [3,5,8] 下 8 轮）、streak 改按轮次语义；副作用
  串行路径 span 改 instrument（跨 await enter 失真）；resume 无 snapshot 时
  durable_seq 以持久化进度为基线；热更新单次解析去双倍开销
- **server/web：SSE 断线恢复三连修**——durable recovery 三态分类（原
  `Some(vec![])` 二态使 EventStore 重放成为死代码，重启后断线事件静默丢失）；
  RehydrateRequired SSE id 填实际 seq（固定 id:0 曾引发全量重放/无限重连
  风暴/僵尸权限弹窗）；NDJSON Undo 落地与 HTTP 对齐（同进程行为分裂消除）、
  NDJSON/ACP Lagged 发送 re-sync 提示（E-14）；ServerPrompter 孤儿条目清理
  （幽灵权限弹窗）；会话空闲 6h 机会式驱逐（长驻 server 内存无界增长）；
  workspace_read 精确读取上限+1 字节（数 GB 文件不再整读进内存）
- **desktop/tui/cli**：keyring 失败降级 credentials 文件 fallback（headless
  Linux 对齐 CLI 行为）；sidecar 进程退出检测 + 前端通知 + 机读端口行
  （解析不再依赖日志文本启发式）；TUI 移除回看双重偏移；serve stdio 三分支
  接入 `--preset`；CLI REPL 退出前生成会话摘要（特性建成未通车收口）
- **context/memory：预算口径修正**——压缩阈值判定计入 system prompt 与
  tool schemas 固定开销（project_doc 可达 8K token 此前漏计）；context_window
  取主对话 provider（原误取 small model 反向虚高）；calibrate 零值护栏；
  Json 工具结果超长裁剪（500KB JSON 不再直接推进 L3/L4 丢历史）；@memory
  BM25 检索槽位端到端接线（生产零写入致检索永不触发的"建成未通车"收口）；
  记忆文件 0600 私有写；long_term 写入 Ask 附内容预览并去除 AllowAlways
- **metrics：gauge 分表覆盖语义**——set_active_sessions 曾累加且渲染为
  counter（双语义错误）；record_mcp_tool_call 进注册表（/metrics 曾永远
  看不到 MCP 计数）；server 日志轮转 max_log_files(7)（磁盘无界增长）；
  TUI 入口安装 tracing subscriber（观测性四入口缺一）

### Changed

- 架构守卫扩容：manifest 扫描含 target-specific/build-dependencies 全部依赖
  表（ARCH-2 盲区封堵）；cli/tui/server/sdk/desktop 五个组合层 crate 补守卫
  （执法 12→17 crate）；KEYRING_SERVICE/ACCOUNT 常量下沉 core 单一事实来源
  （四处复制消除）；web 组件直调 api 层改经 hooks 封装（§8.3 分层令）；删除
  server 零使用的 hyper 直接依赖
- CI/供应链：dist 开启 github-attestations、桌面产物生成 SHA256SUMS；
  dependabot 配置（cargo/actions/npm weekly 分组合并）；dtolnay/rust-toolchain
  @master 改 SHA 钉版；windows-target-check 扩到除 desktop 外全 workspace；
  generated-guard 钩子阻止 cargo test 的 ts-rs 副作用产物误提交（已两次事故）；
  pre-commit 双轨对齐（脚本轨补 typos/generated-guard）
- Tauri CSP/connect-src 维持既有收紧策略；NDJSON 协议新增 UndoReported 响应
  变体（向后兼容）

### Docs

- 幻影约束清零：security.md §4.2 标注已实现、rules.md C-02 四类覆盖面对齐、
  audit.log 示例按 AuditRecord 六字段重写、auth login→cred store、README
  `--tui`→独立二进制、AsyncRewakeSpec 三份文档对齐代码、design §8.6 override/
  fallback 配置键标注未实现、mcp_choices 结构对齐指纹分桶；10 份历史过程文档
  归档 docs/history/（结论以本文档对应审查报告为准）

## [0.3.4] - 2026-08-25

> 本版本为 `docs/project-review-20260825-r2.md` 第二轮全面审查修复版：
> R1'–R6' 六个阶段收口约 70 项新发现（含 3 个 P1 安全/正确性缺陷与多项
> "接线空转"类功能失效），并复核首轮 R1–R7 全部修复的落地质量。

### Security

- **policy：会话级 Allow 缓存与持久化查表同门控**——C-23/C-27 的 restricted
  ask（不可 Always）此前可被同工具此前的 AllowAlways 批准静默击穿（指令性
  auto.md 写入不再弹窗，记忆投毒通道）；目录粒度前缀匹配先词法规范化路径
  （`src/gen/../secret.txt` 不再逃出批准目录）
- **sandbox：Landlock 分级降级**——FS ABI 逐级探测（V3/V2/V1）、网络限制按
  内核能力启用：内核 <6.7 此前每次 spawn 必失败并把用户推向关闭沙箱；
  doctor 如实报告探测 ABI 与网络限制范围；`.git` 文件形式（worktree/
  submodule）纳入 VCS 保护
- **sandbox：Windows Job Object pid 键控**——单槽覆盖不再静默杀死运行中的
  后台进程树；策略快照改 FIFO 队列；ResumeThread 失败不再误报成功
- **policy：shell 黑名单补 Windows cmd 破坏性动词**（del/erase/rd/rmdir/
  move/copy 等）与复合语句控制关键字剥离（`for..; do rm AGENTS.md; done`
  段首动词判定逃逸封堵）；journal 恢复拒绝符号链接穿透；事件流 append
  seq 单调性校验（跨进程重复 fail-closed）；policy.toml 原子写防并发半写

### Fixed

- **core：C-30 沙箱硬熔断强制 TurnEnd**——HardTripped 后本轮立即终止，
  LLM 无视劝阻文本重试到 max_iters 的路径被切断；`RuntimeConfig.tools`
  超时/输出上限三字段接线生效（用户配置不再被硬编码 120s/1MiB 静默截杀）；
  cancel_token 下传 ToolContext；Failed/Err 路径补发 TurnEnd 终结事件；
  库边界 `Result<_, String>` 收敛为具体错误类型
- **providers：Anthropic thinking 输出上限修正**——budget≥8192 时 clamp 到
  8192 使 max_tokens≤budget_tokens 必 400（0.3.3 修复引入的回归），thinking
  路径上限提升至 64K 且保证严格大于 budget；OpenAI 推理系补 top_p gate
- **tools/mcp：mcp serve 只读暴露统一为 SideEffect::None 判据**——shell.run、
  git.apply、web.* 此前绕过 `--expose-write-tools` 无条件直通执行；后台 shell
  淘汰主动终止进程（孤儿进程消除）；web.search 补超时+有界缓冲；
  shell.output 输出经脱敏（C-04 后台旁路消除）；merge 失败保留分支不销毁
  未合并改动副本
- **server/web：懒恢复会话 SSE cursor 按持久化进度播种**——跨重启断线重连
  可走 durable recovery（此前 Last-Event-ID 重连永久黑屏）；insert_session
  TOCTOU 双 sequencer 消除；Web 前端识别 RehydrateRequired 重拉 snapshot；
  Web/Desktop 补齐 /undo 与 /permission-mode 入口；CLI/TUI 渲染 reasoning
  增量（四形态能力矩阵对齐）
- **context/memory：Auto memory 并发丢更新竞态修复**（RMW 全程持锁）；
  async load 与 load_sync 错误语义对齐；压缩审计移到熔断器锁外；
  post-compact read 路径改为压缩前提取（L3/L4 丢弃后注入恒空修复）

### Changed

- CI 三 workflow 补 concurrency 组与 timeout-minutes；pre-push hook 导出
  `MINICODING_PRE_PUSH=1` 激活推送阶段完整门禁；cliff.toml Security 解析器
  移至 catch-all 前（原永不可达）；`[workspace.lints]` 收敛 18 个 crate 的
  clippy deny 声明并使集成测试纳入 pedantic；secrets 正则覆盖 `.env.*`
- 文档体系第二轮对齐：Event 枚举以 `core/runtime/event.rs` 为权威源
  （清除 HookRun/FileUndone 等幽灵变体）、沙箱驱动表按代码事实重写、
  libseccomp 五处失实声明修正、统计口径逐行重算（54/83/66≈204）、
  roadmap 新增 R2 审查遗留立项清单（12 项）

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
