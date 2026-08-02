//! SSRF 防护（`security.md` §3.2）。
//!
//! URL 校验 + DNS 解析后 IP 黑名单检查，防止 LLM 通过域名绕过私有 IP 限制。

use minicoding_core::model::ToolError;
use std::net::IpAddr;

/// 校验 URL 是否安全（SSRF 防护）。
///
/// 拒绝：
/// - 非 http/https scheme；
/// - hostname 解析到 loopback/private/link-local/unspecified IP。
pub async fn validate_url(url: &str) -> Result<(), ToolError> {
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

    // 2. hostname 解析 + IP 黑名单
    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::InvalidInput("URL 缺少 hostname".into()))?;
    let port = parsed.port_or_known_default().unwrap_or(80);

    // 直接 IP 字面量：直接检查
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(&ip) {
            return Err(ToolError::InvalidInput(format!(
                "SSRF 防护：IP `{ip}` 被拒绝（loopback/private/link-local）"
            )));
        }
        return Ok(());
    }

    // 域名：DNS 解析后检查所有 IP
    let addr_str = format!("{host}:{port}");
    let addrs = tokio::net::lookup_host(&addr_str)
        .await
        .map_err(|e| ToolError::Exec(format!("DNS 解析失败 `{host}`: {e}")))?;

    for sa in addrs {
        let ip = sa.ip();
        if is_blocked_ip(&ip) {
            return Err(ToolError::InvalidInput(format!(
                "SSRF 防护：域名 `{host}` 解析到被拒绝的 IP `{ip}`（loopback/private/link-local）"
            )));
        }
    }

    Ok(())
}

/// 判断 IP 是否在 SSRF 黑名单内。
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || is_ipv6_ula(v6) || is_ipv6_link_local(v6)
        }
    }
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
