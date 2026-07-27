//! # minicoding-cli
//!
//! `CLI` frontend：命令行入口。
//!
//! 解析参数、加载配置、构建 `Runtime`、驱动会话、渲染输出。零业务逻辑——所有决策委托
//! `Runtime`；`CLI` 只做 `IO` 与渲染。
//!
//! ## 设计要点
//!
//! - **feature 组装**：`builder.rs` 根据 cargo feature 启用的实现 crate 装配 `Runtime`
//!   （如未启用 `minicoding-sandbox` 则用 core 的 `NoopDriver`）；
//! - **非 TTY 降级**：检测 `stdout.is_terminal()`，非交互时禁 spinner/颜色，权限走
//!   `NonInteractivePrompter`；
//! - **退出码**：成功 0；运行时错误 1；配置错误 2；中断 130。
//!
//! 当前 M0 阶段：仅占位骨架（T-M0-1），`clap` 最小骨架与 `anyhow` 错误出口见 T-M0-5。
//!
//! 详见 `docs/modules.md` §12、`docs/dev-plan.md` T-M0-5。

#![deny(clippy::all, clippy::pedantic)]

fn main() {
    // M0 占位：T-M0-1 仅要求骨架可编译；T-M0-5 补充 clap 最小骨架与退出码约定
    println!("minicoding - terminal AI coding assistant (skeleton)");
}
