//! # minicoding-extension-sdk
//!
//! 扩展作者稳定 API：`Extension` trait + `Registrar` + `ExtensionManifest`。
//!
//! 为第三方扩展作者提供稳定接口，隐藏 `Runtime` 内部细节。扩展可通过 `Registrar`
//! 注册：工具 / Hook / Prompt contributor / 快捷键 / 状态栏项。
//!
//! ## 三类扩展载体
//!
//! 1. **进程内 first-party**（本 crate 实现组合根，M5+）：内置
//!    扩展（Plan/Task/Memory）编译为 crate，通过 `Registrar` 注册；
//! 2. **disk IPC 子进程扩展**（M6+）：外部可执行 + JSON over stdio，通过
//!    `ExtensionHost` 加载；
//! 3. **MCP 远程扩展**（M4+）：通过 `minicoding-mcp` 包装为 `Tool`。
//!
//! ## 设计要点
//!
//! - **Extension-First 架构**：核心只保留 agent loop / hooks / context compaction /
//!   built-in tools；其他能力（skills / mode / goal）通过扩展接入；
//! - **统一 dispatch**：扩展注册的工具仍走 `ToolRegistry` dispatch 路径，确保权限
//!   审计与可观测性一致（C-01/C-02 不被绕过）；
//! - **能力声明**：`ExtensionManifest` 声明 id / version / capabilities / permissions，
//!   `ExtensionHost` 启动时校验权限边界。
//!
//! 当前 M0 阶段：仅占位骨架（T-M0-1），`Extension` trait 定义在 core（M5），
//! SDK 实现见 M5+。
//!
//! 详见 `docs/modules.md` §17、`docs/design.md` §25、`docs/extensions.md`。

#![deny(clippy::all, clippy::pedantic)]
