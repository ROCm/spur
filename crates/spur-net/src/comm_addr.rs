// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Validate and normalize node comm addresses (NodeAddr) used for agent gRPC
//! and inter-node TCP reachability.

use std::net::{IpAddr, ToSocketAddrs};

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CommAddressError {
    #[error("comm address is empty")]
    Empty,
    #[error("comm address {0:?} is not resolvable: {1}")]
    Unresolvable(String, String),
    #[error("comm address {0} is not routable ({1})")]
    Unusable(String, String),
}

/// True when `addr` is loopback or an unspecified address.
pub fn is_loopback_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

/// True when `addr` is unsuitable for inter-node reachability (loopback,
/// unspecified, or link-local).
pub fn is_unusable_comm_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_unspecified() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || v6.is_unicast_link_local(),
    }
}

/// Whether a comm-address string resolves to an address unsuitable for
/// inter-node reachability (loopback, unspecified, or link-local).
pub fn comm_addr_is_unusable(input: &str) -> bool {
    normalize_comm_address(input)
        .map(|resolved| resolved.parse::<IpAddr>().is_ok_and(is_unusable_comm_ip))
        .unwrap_or(false)
}

/// Resolve `input` to a canonical comm address string (IP literal preferred).
///
/// Hostname inputs perform blocking DNS via [`ToSocketAddrs`]. Call from async
/// contexts through [`tokio::task::spawn_blocking`].
pub fn normalize_comm_address(input: &str) -> Result<String, CommAddressError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(CommAddressError::Empty);
    }

    if let Ok(ip) = input.parse::<IpAddr>() {
        return Ok(ip.to_string());
    }

    let addrs: Vec<_> = format!("{input}:0")
        .to_socket_addrs()
        .map_err(|e| CommAddressError::Unresolvable(input.to_string(), e.to_string()))?
        .map(|sa| sa.ip())
        .collect();

    if addrs.is_empty() {
        return Err(CommAddressError::Unresolvable(
            input.to_string(),
            "no addresses returned".into(),
        ));
    }

    if let Some(ip) = addrs.iter().copied().find(|ip| !is_unusable_comm_ip(*ip)) {
        return Ok(ip.to_string());
    }

    Ok(addrs[0].to_string())
}

/// Normalize and optionally reject non-routable comm addresses (loopback,
/// unspecified, or link-local).
pub fn validate_comm_address(
    input: &str,
    reject_loopback: bool,
) -> Result<String, CommAddressError> {
    let normalized = normalize_comm_address(input)?;
    if reject_loopback {
        if let Ok(ip) = normalized.parse::<IpAddr>() {
            if is_unusable_comm_ip(ip) {
                return Err(CommAddressError::Unusable(input.to_string(), normalized));
            }
        }
    }
    Ok(normalized)
}

/// Host portion for `host:port` strings; bracket IPv6 literals.
pub fn comm_host_for_socket(host: &str) -> String {
    if host.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()) {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Format a comm address and port for TCP/gRPC socket connection strings.
pub fn format_comm_socket(host: &str, port: u16) -> String {
    format!("{}:{port}", comm_host_for_socket(host))
}

/// Format a comm address and port for HTTP agent RPC URLs.
pub fn format_comm_http_url(host: &str, port: u16) -> String {
    format!("http://{}:{port}", comm_host_for_socket(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_ip_literal() {
        assert_eq!(normalize_comm_address("10.0.0.1").unwrap(), "10.0.0.1");
    }

    #[test]
    fn normalize_localhost_is_loopback() {
        let addr = normalize_comm_address("127.0.0.1").unwrap();
        assert_eq!(addr, "127.0.0.1");
        assert!(comm_addr_is_unusable("127.0.0.1"));
    }

    #[test]
    fn normalize_localhost_hostname() {
        let addr = normalize_comm_address("localhost").unwrap();
        assert!(addr == "127.0.0.1" || addr == "::1");
    }

    #[test]
    fn reject_loopback_when_configured() {
        assert!(matches!(
            validate_comm_address("127.0.0.1", true),
            Err(CommAddressError::Unusable(_, _))
        ));
        assert_eq!(validate_comm_address("10.0.0.2", true).unwrap(), "10.0.0.2");
    }

    #[test]
    fn allow_loopback_when_not_rejecting() {
        assert_eq!(
            validate_comm_address("127.0.0.1", false).unwrap(),
            "127.0.0.1"
        );
    }

    #[test]
    fn reject_link_local_when_configured() {
        assert!(matches!(
            validate_comm_address("169.254.1.1", true),
            Err(CommAddressError::Unusable(_, _))
        ));
    }

    #[test]
    fn empty_is_rejected() {
        assert!(matches!(
            normalize_comm_address(""),
            Err(CommAddressError::Empty)
        ));
    }

    #[test]
    fn format_comm_socket_brackets_ipv6() {
        assert_eq!(format_comm_socket("::1", 6818), "[::1]:6818");
        assert_eq!(
            format_comm_http_url("2001:db8::1", 6818),
            "http://[2001:db8::1]:6818"
        );
        assert_eq!(format_comm_socket("10.0.0.1", 6818), "10.0.0.1:6818");
    }
}
