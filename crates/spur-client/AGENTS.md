# AGENTS.md -- spur-client

## Overview

Transport-dialing library that establishes a `tonic::transport::Channel` from a
comma-separated list of gRPC endpoints. Service-agnostic: callers wrap the
returned channel in whatever generated client they need (controller, agent, etc.).

Single source file: `src/lib.rs`. No submodules.

## Connection management

`connect_channel(endpoints)` is the primary entry point. It parses the
comma-separated string, normalizes each entry (adds `http://` if no scheme),
and dials them sequentially until one succeeds.

Variants:
- `connect_channel` -- returns `Channel`.
- `connect_channel_tracked` -- returns `(Channel, String)` where the string
  is the endpoint that was actually dialed. Use when error attribution matters.
- `connect_channel_tracked_with_timeouts` -- same, with explicit
  `ChannelTimeouts` instead of defaults.

Once connected, server-side Raft leader forwarding routes writes to the
current leader, so any reachable node suffices.

### ChannelTimeouts

```
connect:              2s   -- TCP dial budget
keep_alive_interval: 10s   -- HTTP/2 keepalive ping interval
keep_alive_timeout:   5s   -- time to wait for keepalive ack
```

Zero means "disable this timeout" (falls back to OS default), not
"instant deadline". `keep_alive_while_idle` is enabled when keepalive is on.

## Failover / retry logic

Endpoints are tried in order. On connection failure the next is attempted.
If all fail, the last error is returned. There is no automatic reconnect or
background health-checking -- this is a one-shot dial.

`deprioritize(endpoints, avoid)` moves a known-bad endpoint to the back of
the list so a subsequent `connect_channel` call tries it last. This is the
caller's mechanism for rotation after an RPC failure on an established channel.

`endpoint_list()` guarantees at least one entry so callers get a transport
error instead of a silent no-op on empty input.

## Authentication

None at the transport layer. The channel is plain `http://` by default.
`https://` is preserved if the caller provides it. No TLS configuration,
mTLS, or token injection happens in this crate.

## API surface

### Public functions

| Function | Signature (simplified) | Returns |
|---|---|---|
| `parse_endpoints` | `(&str) -> Vec<String>` | Normalized endpoint list |
| `connect_channel` | `async (&str) -> Result<Channel>` | First reachable channel |
| `connect_channel_tracked` | `async (&str) -> Result<(Channel, String)>` | Channel + dialed endpoint |
| `connect_channel_tracked_with_timeouts` | `async (&str, ChannelTimeouts) -> Result<(Channel, String)>` | Channel + dialed endpoint |
| `deprioritize` | `(&str, &str) -> String` | Reordered endpoint string |

### Public types

| Type | Purpose |
|---|---|
| `ChannelTimeouts` | Dial and keepalive durations |

## Dependencies

Runtime: `tonic` (transport), `tracing` (debug logs on failover).
Dev: `tokio` (test-util for paused time), `tokio-stream`, `tonic-health`.

## Testing

Tests cover: scheme normalization, empty-input safety, failover across
multiple dead endpoints, keepalive timeout against a stuck peer (paused
tokio time), zero-timeout-means-disable semantics, `deprioritize` rotation,
and tracked-endpoint reporting.

Run with `cargo test -p spur-client`. The stuck-peer test uses
`#[tokio::test(start_paused = true)]` for deterministic keepalive behavior.
