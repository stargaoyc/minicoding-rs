//! 通用小工具（无领域语义的共享骨架）。

pub mod circuit_breaker;
pub mod fs_private;

pub use circuit_breaker::{BreakerState, CircuitBreaker, CircuitBreakerConfig};
pub use fs_private::write_private;

/// 生成 API 鉴权 token（S1）：ULID 两拼接（160bit 表示，80×2 bit 随机）。
///
/// server bin / CLI `serve` / desktop sidecar 三处复用，保证生成策略一致。
#[must_use]
pub fn generate_auth_token() -> String {
    format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new())
}
