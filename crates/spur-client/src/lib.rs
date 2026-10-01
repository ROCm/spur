// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Transport dialing with client-side failover.
//!
//! Establishes a [`tonic::transport::Channel`] from a comma-separated list of
//! endpoints, rotating to the next endpoint when one is unreachable. The helper
//! is service-agnostic: callers wrap the returned channel in whatever generated
//! client they need (e.g. `spur_proto::controller_client(channel)`, which
//! applies the shared `MAX_GRPC_MESSAGE_SIZE` limit).

use std::time::Duration;

use tonic::transport::{Channel, Endpoint};
use tracing::debug;

/// Parse a comma-separated endpoint string into normalized `http(s)://host:port`
/// entries. Whitespace is trimmed and empty entries dropped. Entries without a
/// scheme are prefixed with `http://`.
pub fn parse_endpoints(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalize_endpoint)
        .collect()
}

fn normalize_endpoint(addr: &str) -> String {
    if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    }
}

/// Always yields at least one entry so callers get a real transport error
/// instead of a silent no-op when handed an empty string.
fn endpoint_list(raw: &str) -> Vec<String> {
    let parsed = parse_endpoints(raw);
    if parsed.is_empty() {
        vec![normalize_endpoint(raw.trim())]
    } else {
        parsed
    }
}

/// Connect to the first reachable controller endpoint.
///
/// Endpoints are tried in the order given. On connection failure the next
/// endpoint is attempted; if every endpoint fails, the last error is returned.
/// Once connected to any running controller, server-side leader forwarding
/// routes writes to the current Raft leader, so a single reachable node is
/// sufficient regardless of which one is the leader.
pub async fn connect_channel(endpoints: &str) -> Result<Channel, tonic::transport::Error> {
    connect_channel_tracked(endpoints)
        .await
        .map(|(channel, _)| channel)
}

/// Same as [`connect_channel`], but also returns the endpoint that was actually dialed —
/// a caller blaming a later failure on "the first configured endpoint" would otherwise
/// blame the wrong host whenever an earlier one failed fast and this call fell through.
pub async fn connect_channel_tracked(
    endpoints: &str,
) -> Result<(Channel, String), tonic::transport::Error> {
    let list = endpoint_list(endpoints);
    let last = list.len() - 1;

    for endpoint in &list[..last] {
        match try_connect(endpoint).await {
            Ok(channel) => return Ok((channel, endpoint.clone())),
            Err(e) => debug!(
                %endpoint,
                error = %e,
                "controller endpoint unreachable, trying next"
            ),
        }
    }

    try_connect(&list[last])
        .await
        .map(|channel| (channel, list[last].clone()))
}

/// Dial budget only — the handshake resolves as soon as we send our own preface, so a
/// peer that accepts TCP but never engages dials fine; keepalive below catches that.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Liveness for an established channel: a peer that accepts TCP then stops answering is
/// torn down after roughly the interval plus the timeout, independent of any in-flight RPC.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

async fn try_connect(endpoint: &str) -> Result<Channel, tonic::transport::Error> {
    configure_endpoint(
        endpoint,
        CONNECT_TIMEOUT,
        KEEP_ALIVE_INTERVAL,
        KEEP_ALIVE_TIMEOUT,
    )?
    .connect()
    .await
}

/// Shared builder so tests can exercise the real keepalive wiring with short
/// durations instead of waiting out the production ones.
fn configure_endpoint(
    endpoint: &str,
    connect_timeout: Duration,
    keep_alive_interval: Duration,
    keep_alive_timeout: Duration,
) -> Result<Endpoint, tonic::transport::Error> {
    Ok(Endpoint::from_shared(endpoint.to_string())?
        .connect_timeout(connect_timeout)
        .http2_keep_alive_interval(keep_alive_interval)
        .keep_alive_timeout(keep_alive_timeout)
        .keep_alive_while_idle(true))
}

/// Move `avoid` to the back of the endpoint list, wherever it currently sits, so a
/// caller that just saw it fail doesn't dial it again first. A no-op when it's absent
/// or there's only one endpoint.
pub fn deprioritize(endpoints: &str, avoid: &str) -> String {
    let mut list = parse_endpoints(endpoints);
    if list.len() > 1 {
        if let Some(pos) = list.iter().position(|e| e == avoid) {
            let bad = list.remove(pos);
            list.push(bad);
        }
    }
    list.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_endpoint() {
        assert_eq!(
            parse_endpoints("http://ctrl:6817"),
            vec!["http://ctrl:6817"]
        );
    }

    #[test]
    fn parse_adds_scheme() {
        assert_eq!(parse_endpoints("ctrl:6817"), vec!["http://ctrl:6817"]);
    }

    #[test]
    fn parse_preserves_https() {
        assert_eq!(
            parse_endpoints("https://ctrl:6817"),
            vec!["https://ctrl:6817"]
        );
    }

    #[test]
    fn parse_comma_list_with_whitespace() {
        assert_eq!(
            parse_endpoints(" ctrl1:6817 , http://ctrl2:6817 ,ctrl3:6817"),
            vec![
                "http://ctrl1:6817",
                "http://ctrl2:6817",
                "http://ctrl3:6817"
            ]
        );
    }

    #[test]
    fn parse_drops_empty_entries() {
        assert_eq!(
            parse_endpoints("ctrl1:6817,,ctrl2:6817,"),
            vec!["http://ctrl1:6817", "http://ctrl2:6817"]
        );
    }

    #[test]
    fn endpoint_list_never_empty() {
        assert_eq!(endpoint_list(""), vec!["http://"]);
        assert_eq!(endpoint_list("   "), vec!["http://"]);
    }

    #[tokio::test]
    async fn connect_all_down_returns_error() {
        // Two ports that are bound then released, so connections are refused.
        let down1 = free_addr().await;
        let down2 = free_addr().await;
        let endpoints = format!("http://{down1},http://{down2}");
        assert!(connect_channel(&endpoints).await.is_err());
    }

    #[tokio::test]
    async fn connect_rotates_to_first_reachable() {
        let down = free_addr().await;
        let up = spawn_server().await;
        let endpoints = format!("http://{down},http://{up}");
        assert!(connect_channel(&endpoints).await.is_ok());
    }

    #[tokio::test]
    async fn connect_rotates_past_multiple_dead() {
        let down1 = free_addr().await;
        let down2 = free_addr().await;
        let up = spawn_server().await;
        let endpoints = format!("http://{down1},http://{down2},http://{up}");
        assert!(connect_channel(&endpoints).await.is_ok());
    }

    #[test]
    fn deprioritize_moves_the_failed_first_endpoint_to_the_back() {
        assert_eq!(
            deprioritize("http://a:1,http://b:1,http://c:1", "http://a:1"),
            "http://b:1,http://c:1,http://a:1"
        );
    }

    #[test]
    fn deprioritize_moves_a_failed_middle_endpoint_to_the_back() {
        assert_eq!(
            deprioritize("http://a:1,http://b:1,http://c:1", "http://b:1"),
            "http://a:1,http://c:1,http://b:1"
        );
    }

    #[test]
    fn deprioritize_is_a_no_op_when_the_endpoint_is_absent() {
        assert_eq!(
            deprioritize("http://a:1,http://b:1", "http://z:1"),
            "http://a:1,http://b:1"
        );
    }

    #[test]
    fn deprioritize_is_a_no_op_with_a_single_endpoint() {
        assert_eq!(deprioritize("http://a:1", "http://a:1"), "http://a:1");
    }

    #[tokio::test]
    async fn connect_channel_tracked_reports_the_endpoint_it_actually_dialed() {
        let down = free_addr().await;
        let up = spawn_server().await;
        let endpoints = format!("http://{down},http://{up}");
        let (_channel, dialed) = connect_channel_tracked(&endpoints)
            .await
            .expect("second endpoint is reachable");
        assert_eq!(dialed, format!("http://{up}"));
    }

    /// A peer that accepts TCP and never engages at the app layer must not hang an
    /// RPC forever; the outer timeout is a bound a regression would blow through.
    #[tokio::test]
    async fn stuck_peer_rpc_errors_out_via_keepalive_instead_of_hanging() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _stream = stream;
            std::future::pending::<()>().await
        });

        let channel = configure_endpoint(
            &format!("http://{addr}"),
            Duration::from_secs(2),
            Duration::from_millis(200),
            Duration::from_millis(200),
        )
        .expect("valid endpoint")
        .connect()
        .await
        .expect("dial succeeds even though the peer is silent");

        let mut client = tonic_health::pb::health_client::HealthClient::new(channel);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.check(tonic_health::pb::HealthCheckRequest::default()),
        )
        .await
        .expect("keepalive must bound the hung RPC, not let it hang forever");
        assert!(result.is_err(), "RPC against a silent peer must fail");
    }

    /// Bind to an ephemeral port and release it, yielding an address that will
    /// refuse connections.
    async fn free_addr() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        listener.local_addr().expect("local addr")
    }

    /// Start a minimal h2 server on a bound listener and return its address.
    /// The service is irrelevant: `connect_channel` only completes the h2
    /// handshake and returns a channel without issuing an RPC.
    async fn spawn_server() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let (_reporter, health) = tonic_health::server::health_reporter();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(health)
                .serve_with_incoming(incoming)
                .await;
        });
        addr
    }
}
