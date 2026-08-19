//! 通用小工具（无领域语义的共享骨架）。

pub mod circuit_breaker;

pub use circuit_breaker::{BreakerState, CircuitBreaker, CircuitBreakerConfig};
