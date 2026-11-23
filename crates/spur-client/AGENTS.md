# AGENTS.md -- spur-client

Service-agnostic dialing: returns a `tonic::transport::Channel`; callers wrap it in the generated client they need.

## Pitfalls

- Failover happens only at dial time. Once connected, the `Channel` reconnects to the same endpoint on transport errors and never fails over to another one. To rotate after an RPC failure on an established channel, call `deprioritize` and dial again.
- Use `connect_channel_tracked*` when attributing errors to a host. An earlier endpoint may have failed and the dial fallen through to a later one.
- `ChannelTimeouts` zero values are not uniform:
  - zero `connect`: no connect timeout (unbounded dial).
  - zero `keep_alive_interval`: keepalive disabled, no pings sent.
  - zero `keep_alive_timeout`: not set on the channel, so the tonic/hyper default applies. It does not disable keepalive.
- `endpoint_list` never returns empty, so empty input yields a transport error rather than a silent no-op. Preserve this.
- No TLS, mTLS, or token injection happens here. `https://` is passed through but nothing configures TLS.

## Testing

- The stuck-peer keepalive test relies on `#[tokio::test(start_paused = true)]` for determinism; keepalive tests need paused tokio time.
