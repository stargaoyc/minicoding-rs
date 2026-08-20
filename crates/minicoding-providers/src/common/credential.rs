//! 凭证重解析器（M-10，R-07）：每次请求重解析凭证，缓存 ≤TTL，换 key 零重启。
//!
//! 设计目标（`design.md` §7、`security.md` §2.5）：
//! - provider 不再持有 `api_key: String`（构造期一次性快照），改经 `resolve` 每次
//!   请求读取——keyring/env 中凭证变更后 ≤TTL 内新请求自动生效；
//! - TTL 缓存避免每次请求都打 keyring（keyring 是进程外服务，高频读有性能/稳定性
//!   代价）；`invalidate` 供"保存配置后"立即清缓存（零等待生效）；
//! - C-04 不放松：凭证仅存内存缓存与 OS keyring/env，不落盘明文，不做日志。
//!
//! 语义：`resolve` 命中未过期缓存直接返回；过期后调用注入的 `loader` 重读，
//! loader 返回 `None`（如 CLI 一次性 `--api-key`，keyring/env 无来源）时**保留旧
//! 缓存继续用**——避免构造期 seed 的凭证在 TTL 后失效，导致进程内 key 无法再用。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use minicoding_core::model::LlmError;

/// 单条缓存的凭证。
#[derive(Debug, Clone)]
struct CachedCred {
    key: String,
    fetched_at: Instant,
}

/// 凭证重读回调：`provider` 名 → 凭证（`None` 表示无来源，如 CLI 一次性 key）。
pub type CredentialLoader = Arc<dyn Fn(&str) -> Result<Option<String>, LlmError> + Send + Sync>;

/// 凭证重解析器（线程安全，可 `Arc` 共享）。
///
/// `loader` 决定重读来源（keyring/env/配置），由调用方（CLI/server/desktop）注入；
/// 默认（`CredentialResolver::from_env`）从 `{PROVIDER}_API_KEY` 环境变量读取。
pub struct CredentialResolver {
    cache: Mutex<HashMap<String, CachedCred>>,
    ttl: Duration,
    loader: CredentialLoader,
}

impl std::fmt::Debug for CredentialResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialResolver")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl CredentialResolver {
    /// 构造解析器（默认 TTL 60s）。
    pub fn new(ttl: Duration, loader: CredentialLoader) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            ttl,
            loader,
        }
    }

    /// 从环境变量构造解析器：`loader` 读 `{PROVIDER}_API_KEY`（大写，`-`→`_`）。
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(
            DEFAULT_TTL,
            Arc::new(|provider: &str| {
                let var = format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"));
                Ok(std::env::var(&var).ok())
            }),
        )
    }

    /// 构造期预填充缓存（已知 key 直接生效，如 CLI `--api-key`/server keyring 读取）。
    ///
    /// # Panics
    /// 内部缓存锁被污染时 panic（构造期单线程使用，正常不会发生）。
    pub fn seed(&self, provider: &str, key: String) {
        self.cache.lock().expect("cache poisoned").insert(
            provider.to_string(),
            CachedCred {
                key,
                fetched_at: Instant::now(),
            },
        );
    }

    /// 解析凭证：缓存命中且未过期直接返回；否则 `loader` 重读。
    ///
    /// `loader` 返回 `None` 且缓存有旧值时保留旧值（见模块注释语义），
    /// 缓存与 loader 均无值时返回 `Ok(None)`。
    ///
    /// # Errors
    /// `loader` 读取失败（keyring 不可用等）时返回 `LlmError`。
    ///
    /// # Panics
    /// 内部缓存锁被污染时 panic（正常不会发生）。
    pub fn resolve(&self, provider: &str) -> Result<Option<String>, LlmError> {
        {
            let cache = self.cache.lock().expect("cache poisoned");
            if let Some(c) = cache
                .get(provider)
                .filter(|c| c.fetched_at.elapsed() <= self.ttl)
            {
                return Ok(Some(c.key.clone()));
            }
        }
        let fresh = (self.loader)(provider)?;
        let mut cache = self.cache.lock().expect("cache poisoned");
        if let Some(key) = fresh {
            cache.insert(
                provider.to_string(),
                CachedCred {
                    key: key.clone(),
                    fetched_at: Instant::now(),
                },
            );
            Ok(Some(key))
        } else if let Some(c) = cache.get(provider) {
            // loader 无来源（一次性 seed 场景）：保留旧缓存继续用
            Ok(Some(c.key.clone()))
        } else {
            Ok(None)
        }
    }

    /// 失效某 provider 的缓存（保存配置/换 key 后调用，下次 `resolve` 立即重读）。
    ///
    /// # Panics
    /// 内部缓存锁被污染时 panic（正常不会发生）。
    pub fn invalidate(&self, provider: &str) {
        self.cache.lock().expect("cache poisoned").remove(provider);
    }
}

/// 默认 TTL（60s）：折中"换 key 生效延迟"与"keyring 读取频率"。
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    fn env_loader() -> CredentialLoader {
        Arc::new(|p: &str| Ok(Some(format!("env-{p}"))))
    }

    #[test]
    fn resolve_hits_cache_within_ttl() {
        let r = CredentialResolver::new(Duration::from_secs(60), env_loader());
        r.seed("openai", "seed-key".into());
        assert_eq!(r.resolve("openai").unwrap().as_deref(), Some("seed-key"));
        // TTL 内 loader 不被调用（loader 返回 env-key，seed 优先命中）
    }

    #[test]
    fn resolve_after_ttl_reruns_loader() {
        let r = CredentialResolver::new(Duration::from_secs(0), env_loader());
        r.seed("openai", "seed-key".into());
        // TTL=0：立即过期 → loader 重读
        assert_eq!(r.resolve("openai").unwrap().as_deref(), Some("env-openai"));
    }

    #[test]
    fn invalidate_forces_reresolve() {
        let r = CredentialResolver::new(Duration::from_secs(60), env_loader());
        r.seed("openai", "seed-key".into());
        r.invalidate("openai");
        assert_eq!(r.resolve("openai").unwrap().as_deref(), Some("env-openai"));
    }

    #[test]
    fn loader_none_keeps_stale_cache() {
        let loader: CredentialLoader = Arc::new(|_| Ok(None));
        let r = CredentialResolver::new(Duration::from_secs(0), loader);
        r.seed("openai", "seed-key".into());
        assert_eq!(r.resolve("openai").unwrap().as_deref(), Some("seed-key"));
    }
}
