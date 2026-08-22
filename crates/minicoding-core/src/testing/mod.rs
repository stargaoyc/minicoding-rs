//! 跨 crate 共享的测试基建（`feature = "test-util"`，M-13/R-09）。
//!
//! 仅测试使用：领域 crate 以 `dev-dependencies + features = ["test-util"]` 引入
//! core，在自己的集成测试中对具体后端运行契约断言。本模块**不是**领域实现——
//! 只含断言逻辑，不引入任何新依赖（架构守卫 `tests/architecture.rs` 白名单不变）。

pub mod manifest_guard;
pub mod storage_contract;
