//! SSRF 防护（T-M4-11，C-02，见 `security.md` §5.1）。
//!
//! **R9 NET-1 标注**：本模块为**死代码**——生产路径使用
//! `minicoding-tools/src/web/ssrf.rs`（`validate_url_resolved` + IP pinning 关闭
//! DNS rebinding TOCTOU，覆盖段更全、异步 DNS）；本模块以公共 API 导出但全仓
//! **零调用点**，且判定更弱（同步阻塞 DNS、无 rebinding 防护、缺
//! `240/4`/`198.18/15`/NAT64/6to4 等段）。保留原因：344 行 + 12 测试的既有
//! API 删除会破坏潜在外部使用方。**新代码不得调用本模块**，请用 tools 版
//! （若需在 policy 侧复用，应先做覆盖段与异步对齐再迁移）。
//!
//! 校验 URL 目标主机是否落在内网/元数据/回环范围，拒绝访问：
//! - RFC1918 私网（`10/8`、`172.16/12`、`192.168/16`）；
//! - 链路本地 `169.254/16`（云元数据接口，AWS/GCP/Azure metadata）；
//! - 回环 `127/8`（除非显式 `allow_loopback`）；
//! - 非公网 IP（`0.0.0.0`、`100.64/10` CGNAT、`::1`、`fc00::/7` ULA、`fe80::/10`）。
//!
//! ## 用法
//!
//! `web.fetch` / MCP HTTP transport 等网络工具在请求前调 `check_url`，命中黑名单
//! 时返回 `SsrfError`，由 policy builtin 转为 `Verdict::Deny`。
//!
//! ## 解析策略
//!
//! 域名先经 `Url::host_str` 取主机名，再尝试 `std::net::ToSocketAddrs` 解析为
//! IP 地址。**不做 DNS 重绑定防护**（M5+ 接入：解析后到连接前再次校验 IP）。
//! 当前仅校验已知私网/元数据 IP 段与字面量 IP。

use std::net::IpAddr;
use std::str::FromStr;

use thiserror::Error;

/// SSRF 校验错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SsrfError {
    /// 目标主机解析失败（DNS 错误或主机名格式无效）。
    #[error("host unresolvable: {0}")]
    Unresolvable(String),
    /// 目标落在 RFC1918 私网。
    #[error("private ip blocked: {0}")]
    PrivateIp(String),
    /// 目标落在链路本地（含云元数据 169.254.169.254）。
    #[error("link-local blocked: {0}")]
    LinkLocal(String),
    /// 目标落在回环（127/8）。
    #[error("loopback blocked: {0}")]
    Loopback(String),
    /// 目标落在非公网段（0.0.0.0 / CGNAT / ULA 等）。
    #[error("non-public ip blocked: {0}")]
    NonPublic(String),
}

/// SSRF 校验选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct SsrfOptions {
    /// 允许回环（`127/8`、`::1`）。用于本地 Ollama 等。
    pub allow_loopback: bool,
    /// 允许私网（RFC1918）。用于内网服务。
    pub allow_private: bool,
}

impl SsrfOptions {
    /// 创建默认严格选项（全拒内网/回环/元数据）。
    #[must_use]
    pub fn strict() -> Self {
        Self::default()
    }

    /// 创建本地开发选项（允许回环，仍拒私网/元数据）。
    #[must_use]
    pub fn local_dev() -> Self {
        Self {
            allow_loopback: true,
            allow_private: false,
        }
    }
}

/// 校验 URL 字符串的 SSRF 安全性。
///
/// 解析 URL 主机名后校验 IP 段。域名先解析为 IP，再判断段位。
///
/// # Errors
/// - `SsrfError::Unresolvable`：URL 解析失败或主机名解析失败；
/// - 其他 `SsrfError`：命中对应黑名单段。
pub fn check_url(url: &str, opts: SsrfOptions) -> Result<(), SsrfError> {
    let parsed = url::Url::parse(url).map_err(|_| SsrfError::Unresolvable(url.to_string()))?;
    // 用 `host()` 而非 `host_str()`：前者返回 `Host` 枚举，IPv6 不带方括号；
    // 后者返回 `[::1]` 字面量，`IpAddr::from_str` 解析失败会误走 DNS 路径。
    match parsed.host() {
        Some(url::Host::Ipv4(v4)) => check_ip(&IpAddr::V4(v4), opts),
        Some(url::Host::Ipv6(v6)) => check_ip(&IpAddr::V6(v6), opts),
        Some(url::Host::Domain(domain)) => check_host(domain, opts),
        None => Err(SsrfError::Unresolvable("no host".to_string())),
    }
}

/// 校验主机名（域名或 IP 字面量）。
///
/// # DNS 重绑定边界（SEC-12，2026-08-28 R5 收尾）
///
/// check 与 connect 各自解析域名——攻击者可让两次解析返回不同 IP（重绑定），
/// 绕过本校验（首查公网 IP 通过、二次解析指向内网）。属文档化的已知边界
/// （规划 M5+ 接入 IP pinning，见 `security.md` §5）；`minicoding-tools::web`
/// 侧已有解析-连接 IP pinning 兜底（A2）。
///
/// # 同步 DNS 语义（SEC-R6-3，2026-08-28 R6 审查）
///
/// 本函数是同步 API——域名解析用 `ToSocketAddrs`（阻塞 DNS）。**生产路径
/// 不使用本函数**：`minicoding-tools::web` 走 `validate_url_resolved`
/// （`tokio::net::lookup_host` 异步解析 + IP pinning）。外部调用方在 async
/// 上下文使用本函数时应在 `spawn_blocking` 中执行，避免阻塞 tokio worker。
///
/// # Errors
/// - `SsrfError::Unresolvable`：域名 DNS 解析失败；
/// - 其他 `SsrfError`：命中对应黑名单段。
pub fn check_host(host: &str, opts: SsrfOptions) -> Result<(), SsrfError> {
    // 1. 先尝试当作 IP 字面量校验
    if let Ok(ip) = IpAddr::from_str(host) {
        return check_ip(&ip, opts);
    }

    // 2. 域名解析为 IP 后校验（不防 DNS 重绑定，M5+ 接入）
    let addrs = match (host, 0).to_socket_addrs() {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(_) => return Err(SsrfError::Unresolvable(host.to_string())),
    };
    for socket_addr in addrs {
        check_ip(&socket_addr.ip(), opts)?;
    }
    Ok(())
}

/// 校验单个 IP 地址是否落在黑名单段。
///
/// # Errors
/// 返回对应的 `SsrfError` 变体表示命中哪类黑名单段（私网/链路本地/回环/非公网）。
pub fn check_ip(ip: &IpAddr, opts: SsrfOptions) -> Result<(), SsrfError> {
    match ip {
        IpAddr::V4(v4) => check_ipv4(*v4, opts),
        IpAddr::V6(v6) => check_ipv6(*v6, opts),
    }
}

fn check_ipv4(ip: std::net::Ipv4Addr, opts: SsrfOptions) -> Result<(), SsrfError> {
    let octets = ip.octets();

    // 0.0.0.0 / 8：当前网络（非公网）
    if octets[0] == 0 {
        return Err(SsrfError::NonPublic(ip.to_string()));
    }

    // 10/8：RFC1918 私网
    if octets[0] == 10 && !opts.allow_private {
        return Err(SsrfError::PrivateIp(ip.to_string()));
    }

    // 172.16/12：RFC1918 私网
    if octets[0] == 172 && (octets[1] & 0xf0) == 0x10 && !opts.allow_private {
        return Err(SsrfError::PrivateIp(ip.to_string()));
    }

    // 192.168/16：RFC1918 私网
    if octets[0] == 192 && octets[1] == 168 && !opts.allow_private {
        return Err(SsrfError::PrivateIp(ip.to_string()));
    }

    // 169.254/16：链路本地（含云元数据 169.254.169.254）
    if octets[0] == 169 && octets[1] == 254 {
        return Err(SsrfError::LinkLocal(ip.to_string()));
    }

    // 127/8：回环
    if octets[0] == 127 && !opts.allow_loopback {
        return Err(SsrfError::Loopback(ip.to_string()));
    }

    // 100.64/10：CGNAT（非公网）
    if octets[0] == 100 && (octets[1] & 0xc0) == 0x40 {
        return Err(SsrfError::NonPublic(ip.to_string()));
    }

    Ok(())
}

fn check_ipv6(ip: std::net::Ipv6Addr, opts: SsrfOptions) -> Result<(), SsrfError> {
    // SEC-1（2026-08-27 R5 审查）：IPv4-mapped IPv6 地址（::ffff:a.b.c.d）
    // 的 `to_ipv4_mapped()` 返回 `Some(Ipv4Addr)`——解析为 `url::Host::Ipv6`
    // 的 `[::ffff:169.254.169.254]` 此前不触发 check_ipv4 的私网/回环/元数据
    // 检查，可直接到达云元数据接口、内网服务、回环端口。已实测复现。
    if let Some(v4) = ip.to_ipv4_mapped() {
        return check_ipv4(v4, opts);
    }

    // ::1 回环
    if ip.is_loopback() && !opts.allow_loopback {
        return Err(SsrfError::Loopback(ip.to_string()));
    }

    // fe80::/10 链路本地
    if (ip.segments()[0] & 0xffc0) == 0xfe80 {
        return Err(SsrfError::LinkLocal(ip.to_string()));
    }

    // fc00::/7 唯一本地地址（ULA，IPv6 私网）
    if (ip.segments()[0] & 0xfe00) == 0xfc00 && !opts.allow_private {
        return Err(SsrfError::PrivateIp(ip.to_string()));
    }

    Ok(())
}

// 把 to_socket_addrs 引入作用域
use std::net::ToSocketAddrs;

// `url` crate：轻量 URL 解析（无网络 IO），见 Cargo.toml。
// policy crate 不直接依赖 reqwest（重依赖），用独立 `url` crate 解析主机名。

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic)]
    use super::*;

    #[test]
    fn cloud_metadata_blocked() {
        let opts = SsrfOptions::strict();
        let result = check_url("http://169.254.169.254/latest/meta-data/", opts);
        assert!(matches!(result, Err(SsrfError::LinkLocal(_))));
    }

    #[test]
    fn rfc1918_blocked() {
        let opts = SsrfOptions::strict();
        assert!(matches!(
            check_url("http://10.0.0.1/", opts),
            Err(SsrfError::PrivateIp(_))
        ));
        assert!(matches!(
            check_url("http://172.16.0.1/", opts),
            Err(SsrfError::PrivateIp(_))
        ));
        assert!(matches!(
            check_url("http://192.168.1.1/", opts),
            Err(SsrfError::PrivateIp(_))
        ));
    }

    #[test]
    fn loopback_blocked_by_default() {
        let opts = SsrfOptions::strict();
        let result = check_url("http://127.0.0.1:8080/", opts);
        assert!(matches!(result, Err(SsrfError::Loopback(_))));
    }

    #[test]
    fn loopback_allowed_in_local_dev() {
        let opts = SsrfOptions::local_dev();
        let result = check_url("http://127.0.0.1:11434/", opts);
        assert!(result.is_ok());
    }

    #[test]
    fn private_allowed_when_explicit() {
        let opts = SsrfOptions {
            allow_loopback: false,
            allow_private: true,
        };
        assert!(check_url("http://10.0.0.1/", opts).is_ok());
        // 元数据仍拒
        assert!(matches!(
            check_url("http://169.254.169.254/", opts),
            Err(SsrfError::LinkLocal(_))
        ));
    }

    #[test]
    fn cgnat_blocked() {
        let opts = SsrfOptions::strict();
        let result = check_url("http://100.64.0.1/", opts);
        assert!(matches!(result, Err(SsrfError::NonPublic(_))));
    }

    #[test]
    fn zero_address_blocked() {
        let opts = SsrfOptions::strict();
        let result = check_url("http://0.0.0.0/", opts);
        assert!(matches!(result, Err(SsrfError::NonPublic(_))));
    }

    #[test]
    fn ipv6_loopback_blocked() {
        let opts = SsrfOptions::strict();
        let result = check_url("http://[::1]:8080/", opts);
        assert!(matches!(result, Err(SsrfError::Loopback(_))));
    }

    #[test]
    fn ipv6_ula_blocked() {
        let opts = SsrfOptions::strict();
        // fc00::1
        let ip: IpAddr = "fc00::1".parse().unwrap();
        let result = check_ip(&ip, opts);
        assert!(matches!(result, Err(SsrfError::PrivateIp(_))));
    }

    #[test]
    fn ipv4_mapped_ipv6_blocked() {
        // SEC-1（R5）：IPv4-mapped IPv6 必须经 check_ipv4 语义拒绝——
        // [::ffff:169.254.169.254]、[::ffff:10.0.0.1]、[::ffff:127.0.0.1]、
        // [::ffff:192.168.1.1] 此前全部放行（is_loopback/is_unicast_link_local
        // 对 mapped 地址均 false，fc00::/7 不命中）
        let opts = SsrfOptions::strict();
        assert!(matches!(
            check_url("http://[::ffff:169.254.169.254]/latest/meta-data/", opts),
            Err(SsrfError::LinkLocal(_))
        ));
        assert!(matches!(
            check_url("http://[::ffff:10.0.0.1]/", opts),
            Err(SsrfError::PrivateIp(_))
        ));
        assert!(matches!(
            check_url("http://[::ffff:127.0.0.1]:8080/", opts),
            Err(SsrfError::Loopback(_))
        ));
        assert!(matches!(
            check_url("http://[::ffff:192.168.1.1]/", opts),
            Err(SsrfError::PrivateIp(_))
        ));
        // allow_loopback 时 mapped 回环放行（与纯 IPv4 语义一致）
        let opts = SsrfOptions::local_dev();
        assert!(check_url("http://[::ffff:127.0.0.1]:11434/", opts).is_ok());
        // 公网 mapped 地址仍放行
        let opts = SsrfOptions::strict();
        assert!(check_url("http://[::ffff:8.8.8.8]/", opts).is_ok());
    }

    #[test]
    fn public_ip_allowed() {
        let opts = SsrfOptions::strict();
        // 8.8.8.8 公网
        assert!(check_url("http://8.8.8.8/", opts).is_ok());
        // 1.1.1.1 公网
        assert!(check_url("http://1.1.1.1/", opts).is_ok());
    }

    #[test]
    fn invalid_url_unresolvable() {
        let opts = SsrfOptions::strict();
        let result = check_url("not-a-url", opts);
        assert!(matches!(result, Err(SsrfError::Unresolvable(_))));
    }
}
