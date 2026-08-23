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

/// 检测内容是否含指令性模式（C-27：Auto memory 指令注入降级 `Ask`）。
///
/// 单一事实来源（2026-08-23 审查 §8-P2）：此前 `minicoding-policy::builtin`
/// 与 `minicoding-memory::auto` 各持一份同语义实现（领域 crate 不交叉的约束
/// 下只能复制），任一侧加规则另一侧不同步即出现旁路。两 crate 均依赖 core，
/// 下沉至此零新增依赖边。
///
/// 命中任一模式返回 `true`：
/// - 英文祈使/模态（行首，忽略大小写）：`Always use`/`Never`/`Must`/
///   `Do not`/`Don't`/`Should`；
/// - 中文祈使（行首）：`总是`/`永远`/`禁止`/`必须`/`不要`/`不得`/`应当`/`应`；
/// - `AGENTS.md` 风格 section 头：`## Rules`/`## Constraints`/`## 规则`/`## 约束`。
#[must_use]
pub fn contains_directive(content: &str) -> bool {
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("always use")
            || lower.starts_with("never ")
            || lower.starts_with("must ")
            || lower.starts_with("do not ")
            || lower.starts_with("don't ")
            || lower.starts_with("should ")
        {
            return true;
        }
        if line.starts_with("总是")
            || line.starts_with("永远")
            || line.starts_with("禁止")
            || line.starts_with("必须")
            || line.starts_with("不要")
            || line.starts_with("不得")
            || line.starts_with("应当")
            || line.starts_with("应")
        {
            return true;
        }
        if lower.starts_with("## rules")
            || lower.starts_with("## constraints")
            || line.starts_with("## 规则")
            || line.starts_with("## 约束")
        {
            return true;
        }
    }
    false
}

/// 测试用当前时间戳（`OffsetDateTime`，供无 `time` 依赖的下游 crate 测试构造
/// `Message` 使用；生产路径一律 `OffsetDateTime::now_utc()`）。
#[must_use]
pub fn test_now_utc() -> time::OffsetDateTime {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    time::OffsetDateTime::from_unix_timestamp(unix.as_secs().cast_signed())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}
