//! # minicoding-core
//!
//! 抽象层 + `Runtime` 编排（零实现）。
//!
//! 仅含：数据模型、核心 trait 定义、`Runtime` 聚合根与 `Agent` 循环、事件总线、配置、
//! `OTel` 初始化与 span 辅助、路径约定。不含任何领域实现逻辑（压缩算法、黑名单正则、
//! landlock ruleset、rmcp 调用、`JSONL` 写入等）。
//!
//! 依赖方向：core 不依赖任何领域 crate；领域 crate 依赖 core。
//! 依赖约束：仅"轻量 + 无平台/网络"的依赖（见 `modules.md` §1.4、§15.6）。
//!
//! 详见 `docs/modules.md` §1、`docs/design.md` §1。

#![deny(clippy::all, clippy::pedantic)]
