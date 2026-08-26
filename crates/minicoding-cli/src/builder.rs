//! Runtime 组装（A11：实现下沉 `minicoding-sdk::builder`，本文件仅路径兼容 re-export）。
//!
//! cli 内部（main/exec/serve）与 tui 原经 `minicoding_cli::builder` 的调用点不变。

pub use minicoding_sdk::builder::{SessionLoadMode, build_runtime, build_runtime_with_memory_slot};
