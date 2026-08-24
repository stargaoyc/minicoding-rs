//! 测试支撑（仅 `cfg(test)` 编入测试二进制）。
//!
//! 同一 crate 的测试并行运行，而环境变量是**进程级全局**——多个模块的测试
//! 若各自 set/remove `MINICODING_HOME`（`session_mgr` 与 http 的会话创建类测试
//! 都需要磁盘隔离），必须共用同一把锁串行化，否则互相踩踏导致偶发失败。
//!
//! 2026-08-25 审查 F-routes 引入：原 `ENV_LOCK`/`EnvGuard` 私有于
//! `session_mgr.rs` 测试模块，http 端点级测试需要同样的隔离，故上移共享。

/// 串行化所有依赖 `MINICODING_HOME` 的测试。`tokio::sync::Mutex`：guard 需跨
/// `.await` 持有（seed 磁盘数据、HTTP oneshot 均为 async），std Mutex 会触发
/// `clippy::await_holding_lock`。
pub static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 测试期间临时设置 `MINICODING_HOME`，`Drop` 时恢复原值。
pub struct EnvGuard {
    original: Option<String>,
}

impl EnvGuard {
    /// 设置隔离目录（通常为 tempfile 路径），返回恢复 guard。
    pub fn set(value: &str) -> Self {
        let original = std::env::var("MINICODING_HOME").ok();
        // SAFETY: ENV_LOCK 串行化所有 MINICODING_HOME 访问，无并发 set/remove。
        unsafe { std::env::set_var("MINICODING_HOME", value) };
        Self { original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: 同 `set()`，ENV_LOCK 保证串行访问；Drop 在测试 scope 结束时同步调用。
        match &self.original {
            Some(v) => unsafe { std::env::set_var("MINICODING_HOME", v) },
            None => unsafe { std::env::remove_var("MINICODING_HOME") },
        }
    }
}
