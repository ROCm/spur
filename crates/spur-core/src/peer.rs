// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! One definition of how a caller's address is rendered, so the controller,
//! the agent, and the operator cannot log or store it in differing forms.

use std::net::{IpAddr, SocketAddr};

/// A dual-stack listener reports an IPv4 client as `[::ffff:a.b.c.d]:port`,
/// which nobody would search for. Record the plain IPv4 form instead.
pub fn canonical_peer(addr: SocketAddr) -> String {
    match addr.ip() {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), addr.port()).to_string(),
            None => addr.to_string(),
        },
        IpAddr::V4(_) => addr.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every IPv4 caller arrives mapped on the deployed dual-stack listener,
    /// and storing that form made the documented `Peer=<ipv4>` query return nothing.
    #[test]
    fn canonical_peer_unwraps_an_ipv4_mapped_client() {
        let mapped: SocketAddr = "[::ffff:10.11.99.183]:45218".parse().unwrap();
        assert_eq!(canonical_peer(mapped), "10.11.99.183:45218");

        let plain: SocketAddr = "10.11.99.183:45218".parse().unwrap();
        assert_eq!(canonical_peer(plain), "10.11.99.183:45218");
    }

    #[test]
    fn canonical_peer_leaves_a_real_ipv6_client_bracketed() {
        // Genuine IPv6 keeps `SocketAddr`'s bracketed form, which the audit
        // query matches with its bracketed arm.
        let v6: SocketAddr = "[2001:db8::1]:6817".parse().unwrap();
        assert_eq!(canonical_peer(v6), "[2001:db8::1]:6817");
    }
}
