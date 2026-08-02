//! 扩展系统 trait 定义（见 `design.md` §23、`api.md` §3.12）。
//!
//! 三类扩展载体（`ExtensionCarrier`）统一实现 `Extension` trait，Runtime 不感知
//! 载体差异：
//! - `Bundled`：进程内 first-party（`minicoding-extension-sdk` 实现组合根）；
//! - `Ipc { path }`：disk IPC 子进程（`minicoding-cli` 加载器）；
//! - `Mcp { server_id }`：远程扩展（复用 `minicoding-mcp`）。
//!
//! `ExtensionHost` 由 Runtime 持有 `Arc<dyn ExtensionHost>`，在启动时批量加载
//! `~/.minicoding/extensions/` 下的扩展。`NoopExtensionHost` 兜底未启用扩展时的场景。
//!
//! **安全约束**（§23）：扩展注册的工具仍走 `ToolRegistry::dispatch`，权限检查
//! （C-01）与内置黑名单（C-02）对扩展工具同样生效；`ExtensionHost::load_extension`
//! 静态校验 manifest 的 `permissions` 字段；IPC/MCP 扩展的输入经 `sandbox_path`。

pub mod manifest;
pub mod trait_def;

pub use manifest::{
    Capability, ExtensionCarrier, ExtensionId, ExtensionInfo, ExtensionManifest, KeyBinding,
    Permission, SlashCommand, StatusItem,
};
pub use trait_def::{Extension, ExtensionHost, NoopExtensionHost, NoopRegistrar, Registrar};
