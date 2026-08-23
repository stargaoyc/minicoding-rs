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

[Unreleased]: 后续变更见各 commit（Conventional Commits）。
