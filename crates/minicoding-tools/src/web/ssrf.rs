//! SSRF 防护（`security.md` §3.2）。
//!
//! URL 校验 + DNS 解析后 IP 黑名单检查，防止 LLM 通过域名绕过私有 IP 限制。
//!
//! A2：提供 [`validate_ip`]/[`resolve_and_validate_host`] 两层原语供
//! web.fetch 的 DNS 解析-连接 IP pinning 复用（校验与连接钉住用同一套判定，
//! 关闭"校验时解析 A、连接时解析 B"的 rebinding 窗口）。

use minicoding_core::model::ToolError;
use std::net::IpAddr;

/// 校验 URL 是否安全（SSRF 防护）。
///
/// 拒绝：
/// - 非 http/https scheme；
/// - hostname 解析到 loopback/private/link-local/unspecified IP。
pub async fn validate_url(url: &str) -> Result<(), ToolError> {
    validate_url_resolved(url).await.map(|_| ())
}

/// 校验 URL 并返回 hostname 解析出的**全部已校验 IP**（A2 pinning 用）。
///
/// 与 [`validate_url`] 同一套判定；返回值供调用方从中选取钉住 IP——保证
/// "校验所见的解析结果"与"实际连接目标"来自同一次解析，消除 TOCTOU 窗口。
pub(crate) async fn validate_url_resolved(url: &str) -> Result<Vec<IpAddr>, ToolError> {
    let parsed =
        url::Url::parse(url).map_err(|e| ToolError::InvalidInput(format!("URL 解析失败: {e}")))?;

    // 1. scheme 白名单
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(ToolError::InvalidInput(format!(
                "不允许的 scheme `{other}`（仅 http/https）"
            )));
        }
    }

    // 2. hostname 解析 + IP 黑名单（fail-closed）
    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::InvalidInput("URL 缺少 hostname".into()))?;
    let port = parsed.port_or_known_default().unwrap_or(80);
    resolve_and_validate_host(host, port).await
}

/// 校验单个 IP（SSRF 黑名单）。
///
/// # Errors
/// IP 落在黑名单段（loopback/private/link-local/unspecified 及嵌 IPv4 变体）
/// 时返回 `ToolError::InvalidInput`。
pub(crate) fn validate_ip(ip: &IpAddr) -> Result<(), ToolError> {
    if is_blocked_ip(ip) {
        return Err(ToolError::InvalidInput(format!(
            "SSRF 防护：IP `{ip}` 被拒绝（loopback/private/link-local）"
        )));
    }
    Ok(())
}

/// 解析 host 并逐一校验全部结果，返回去重后的合规 IP 列表。
///
/// IP 字面量直接校验返回（不做 DNS）；域名经系统解析器取**全部**地址，任一
/// IP 违规即拒绝整次请求（fail-closed，防多记录部分投毒）。
///
/// # Errors
/// DNS 解析失败、无可用地址或任一 IP 违规时返回 `ToolError`。
pub(crate) async fn resolve_and_validate_host(
    host: &str,
    port: u16,
) -> Result<Vec<IpAddr>, ToolError> {
    // 直接 IP 字面量：直接检查
    if let Ok(ip) = host.parse::<IpAddr>() {
        validate_ip(&ip)?;
        return Ok(vec![ip]);
    }

    // 域名：DNS 解析后检查所有 IP
    let addr_str = format!("{host}:{port}");
    let addrs = tokio::net::lookup_host(&addr_str)
        .await
        .map_err(|e| ToolError::Exec(format!("DNS 解析失败 `{host}`: {e}")))?;

    let mut ips: Vec<IpAddr> = Vec::new();
    for sa in addrs {
        validate_ip(&sa.ip())?;
        if !ips.contains(&sa.ip()) {
            ips.push(sa.ip());
        }
    }
    if ips.is_empty() {
        return Err(ToolError::Exec(format!("DNS 解析 `{host}` 无可用地址")));
    }
    Ok(ips)
}

/// 判断 IP 是否在 SSRF 黑名单内。
///
/// 2026-08-23 审查 §9-P1 增强：
/// - **IPv4-mapped IPv6**（`::ffff:169.254.169.254` 等）先解包为 IPv4 再走
///   v4 检查——内核 dual-stack socket 会将其映射为目标 IPv4，此前全部放行，
///   云元数据/内网直达；
/// - NAT64（`64:ff9b::/96`）与 6to4（`2002::/16`）末 32 位嵌 IPv4，同法解包；
/// - 补 CGNAT（`100.64/10`）与运营商基准测试（`198.18/15`）段。
fn is_blocked_ip(ip: &IpAddr) -> bool {
    // 嵌 IPv4 的 IPv6 形态：解包后按 v4 检查
    if let IpAddr::V6(v6) = ip
        && let Some(v4) = embedded_ipv4(v6)
    {
        return is_blocked_ip(&IpAddr::V4(v4));
    }
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || is_cgnat(*v4)
                || is_benchmarking(*v4)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || is_ipv6_ula(v6)
                || is_ipv6_link_local(v6)
                || is_ipv4_mapped(v6)
                || is_nat64(v6)
                || is_6to4(v6)
        }
    }
}

/// IPv4-mapped IPv6（`::ffff:0:0/96`）检测。
fn is_ipv4_mapped(addr: &std::net::Ipv6Addr) -> bool {
    let segs = addr.segments();
    segs[0..5] == [0, 0, 0, 0, 0] && segs[5] == 0xFFFF
}

/// NAT64 前缀（`64:ff9b::/96`，RFC 6052）检测。
fn is_nat64(addr: &std::net::Ipv6Addr) -> bool {
    let segs = addr.segments();
    segs[0] == 0x64 && segs[1] == 0xFF9B && segs[2..6] == [0, 0, 0, 0]
}

/// 6to4（`2002::/16`）检测。
fn is_6to4(addr: &std::net::Ipv6Addr) -> bool {
    addr.segments()[0] == 0x2002
}

/// 从 mapped/NAT64/6to4 形态的 IPv6 中解包嵌入的 IPv4。
fn embedded_ipv4(addr: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    // u16 → [hi, lo] 字节提取：移位后 & 0xFF，无截断语义
    fn octets(hi: u16, lo: u16) -> std::net::Ipv4Addr {
        std::net::Ipv4Addr::new(
            (hi >> 8) as u8,
            (hi & 0xFF) as u8,
            (lo >> 8) as u8,
            (lo & 0xFF) as u8,
        )
    }
    let segs = addr.segments();
    if is_ipv4_mapped(addr) || is_nat64(addr) {
        return Some(octets(segs[6], segs[7]));
    }
    if is_6to4(addr) {
        return Some(octets(segs[1], segs[2]));
    }
    None
}

/// CGNAT（`100.64/10`，RFC 6598）——运营商级内网，云元数据场景同样不可达外网。
fn is_cgnat(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (o[1] & 0xC0) == 64
}

/// 运营商基准测试保留段（`198.18/15`，RFC 2544）。
fn is_benchmarking(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 198 && (o[1] & 0xFE) == 18
}

/// IPv6 Unique Local Address（`fc00::/7`）检查。
fn is_ipv6_ula(addr: &std::net::Ipv6Addr) -> bool {
    let segs = addr.segments();
    (segs[0] & 0xFE00) == 0xFC00
}

/// IPv6 Link-Local（`fe80::/10`）检查。
fn is_ipv6_link_local(addr: &std::net::Ipv6Addr) -> bool {
    let segs = addr.segments();
    (segs[0] & 0xFFC0) == 0xFE80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ip_accepts_public_and_rejects_private() {
        assert!(validate_ip(&"8.8.8.8".parse().unwrap()).is_ok());
        assert!(validate_ip(&"2606:4700::1".parse().unwrap()).is_ok());
        assert!(validate_ip(&"10.0.0.1".parse().unwrap()).is_err());
        assert!(validate_ip(&"127.0.0.1".parse().unwrap()).is_err());
        // IPv4-mapped IPv6 元数据地址同样拒绝（A2 pinning 复用入口）
        assert!(
            validate_ip(&"::ffff:169.254.169.254".parse().unwrap()).is_err(),
            "mapped 元数据 IP 必须拒绝"
        );
    }

    #[tokio::test]
    async fn resolve_and_validate_host_rejects_loopback_name() {
        // localhost 在常规环境解析到 127.0.0.1/::1——均属黑名单，应 fail-closed；
        // 即使解析失败（受限环境）同样返回 Err
        assert!(resolve_and_validate_host("localhost", 80).await.is_err());
    }

    #[test]
    fn blocks_ipv4_mapped_ipv6_metadata() {
        // ::ffff:169.254.169.254 → 云元数据；此前绕过全部检查
        assert!(is_blocked_ip(&"::ffff:169.254.169.254".parse().unwrap()));
        assert!(is_blocked_ip(&"::ffff:10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn blocks_nat64_and_6to4_embeddings() {
        // NAT64 64:ff9b::a.b.c.d / 6to4 2002:a00:1::
        assert!(is_blocked_ip(&"64:ff9b::127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"2002:7f00:1::".parse().unwrap())); // 127.0.0.1
    }

    #[test]
    fn blocks_cgnat_and_benchmarking() {
        assert!(is_blocked_ip(&"100.100.1.1".parse().unwrap()));
        assert!(is_blocked_ip(&"198.19.0.1".parse().unwrap()));
        // 公网地址不误伤
        assert!(!is_blocked_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_blocked_ip(&"2606:4700:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn validate_url_rejects_non_http_scheme() {
        assert!(validate_url("ftp://example.com").await.is_err());
        assert!(validate_url("file:///etc/passwd").await.is_err());
        assert!(validate_url("javascript:alert(1)").await.is_err());
    }

    #[tokio::test]
    async fn validate_url_rejects_loopback_ipv4() {
        assert!(validate_url("http://127.0.0.1/").await.is_err());
        assert!(validate_url("http://127.0.0.1:8080/").await.is_err());
    }

    #[tokio::test]
    async fn validate_url_rejects_private_ipv4() {
        assert!(validate_url("http://10.0.0.1/").await.is_err());
        assert!(validate_url("http://172.16.0.1/").await.is_err());
        assert!(validate_url("http://192.168.1.1/").await.is_err());
    }

    #[tokio::test]
    async fn validate_url_rejects_link_local_ipv4() {
        assert!(validate_url("http://169.254.1.1/").await.is_err());
    }

    #[tokio::test]
    async fn validate_url_rejects_unspecified_ipv4() {
        assert!(validate_url("http://0.0.0.0/").await.is_err());
    }

    #[tokio::test]
    async fn validate_url_rejects_loopback_ipv6() {
        assert!(validate_url("http://[::1]/").await.is_err());
    }

    #[tokio::test]
    async fn validate_url_rejects_unspecified_ipv6() {
        assert!(validate_url("http://[::]/").await.is_err());
    }

    #[tokio::test]
    async fn validate_url_rejects_invalid_url() {
        assert!(validate_url("not a url").await.is_err());
        assert!(validate_url("").await.is_err());
    }

    #[test]
    fn is_blocked_ip_detects_loopback_v4() {
        assert!(is_blocked_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"127.255.255.255".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_detects_private_v4() {
        assert!(is_blocked_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"192.168.0.1".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_detects_link_local_v4() {
        assert!(is_blocked_ip(&"169.254.0.1".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_detects_unspecified_v4() {
        assert!(is_blocked_ip(&"0.0.0.0".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_detects_broadcast_v4() {
        assert!(is_blocked_ip(&"255.255.255.255".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_detects_documentation_v4() {
        assert!(is_blocked_ip(&"203.0.113.1".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_allows_public_v4() {
        assert!(!is_blocked_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_blocked_ip(&"1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_detects_loopback_v6() {
        assert!(is_blocked_ip(&"::1".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_detects_unspecified_v6() {
        assert!(is_blocked_ip(&"::".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_allows_public_v6() {
        assert!(!is_blocked_ip(&"2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn is_ipv6_ula_detects_fc00_range() {
        assert!(is_ipv6_ula(&"fc00::1".parse().unwrap()));
        assert!(is_ipv6_ula(&"fd00::1".parse().unwrap()));
        assert!(is_ipv6_ula(&"fdff:ffff::1".parse().unwrap()));
    }

    #[test]
    fn is_ipv6_ula_rejects_public() {
        assert!(!is_ipv6_ula(&"2606:4700::1".parse().unwrap()));
    }

    #[test]
    fn is_ipv6_link_local_detects_fe80_range() {
        assert!(is_ipv6_link_local(&"fe80::1".parse().unwrap()));
        assert!(is_ipv6_link_local(&"febf::1".parse().unwrap()));
    }

    #[test]
    fn is_ipv6_link_local_rejects_public() {
        assert!(!is_ipv6_link_local(&"2606:4700::1".parse().unwrap()));
    }
}
