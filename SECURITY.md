# 安全政策

`minicoding-rs` 是一个终端 AI 编码助手——Agent 可执行文件写入、Shell 命令、
网络请求等高权限操作。安全是我们的核心关注点。

## 支持的版本

| 版本 | 支持 |
|------|------|
| 最新发布（v0.3.x） | ✅ 安全修复 |
| 旧版本 | ⚠️ 仅安全公告，不主动修复 |

## 报告漏洞

请**不要**在 GitHub Issues 中公开披露漏洞细节。请直接联系维护者：

- GitHub：https://github.com/stargaoyc/minicoding-rs
- 首选：在仓库创建**私有** issue 并标注 `[SECURITY]`，或直接向维护者
  发送邮件/GitHub 私信（若联系方式已公开）

报告时请包含：

1. 影响版本与复现步骤
2. 漏洞类型与严重性评估（可参考 [OWASP](https://owasp.org/www-community/vulnerability/)）
3. 影响面（如：越权执行 / 凭证泄露 / 沙箱绕过 / 提示注入）

## 处理流程

1. 维护者确认漏洞并分配编号；
2. 在**私有**分支修复并补回归测试；
3. 修复合并后发布安全版本，同时发布安全公告（CHANGELOG + release notes）；
4. 公告发布前，漏洞细节保密。

## 安全边界声明（我们不防什么）

本项目的安全模型建立在"应用层权限 + OS 级沙箱"两道防线上，但有以下明确边界：

- **Windows 沙箱是进程遏制（Job Object），不是安全边界**——受限令牌 /
  DACL / AppContainer 未实现，Windows 上请依赖容器或虚拟机隔离；
- **Linux 沙箱网络面在内核 <6.7 不完整**——UDP/DNS/ICMP 永不受限；
  seccomp 为 opt-in 实验性 feature（默认关）；
- **沙箱不可用时默认 fail-closed**（`minicoding exec` 拒绝执行），交互式
  场景可显式确认沙箱外运行；
- **提示注入无法完全防止**——模型可被恶意内容诱导，但我们保证：工具
  输出有边界转义、副作用需权限确认、凭证不落日志、敏感文件读取脱敏；
- **本地恶意的**进程/用户不在威胁模型内（同机攻击者总能读 `~/.minicoding`）。

## 安全相关设计

- 权限决策落 `~/.minicoding/audit.log`（0600，追加写）
- API key 仅存内存 + OS keyring，不下传子进程 env，日志脱敏
- L0 硬约束（黑名单 / Plan 模式只读 / 路径越界）在实现层强制
- 详细威胁模型见 [`docs/security.md`](docs/security.md)

## 依赖安全

- CI 运行 `cargo audit`（RUSTSEC）与 `cargo deny`（许可/来源）
- dependabot 自动提 PR；安全相关升级优先合并
