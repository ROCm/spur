# AGENTS.md -- spurauthd

Thin binary over `spur_core::native_mint`; the mint, wire protocol, and verifier
logic all live in `spur-core`. The credential model, JWKS format, and rotation
are in `docs/knowledge/security/auth-model.md`.

## Invariants

- `spurauthd` only mints `UserRpc` credentials; it never verifies. `spurctld` and
  `spurd` verify and do not listen on the mint socket. All other credential kinds
  are minted by the controller, never through this daemon.
- Caller identity comes only from `SO_PEERCRED`. The mint ignores any identity
  the caller sends; do not add a request field that influences `user`/`uid`/`gid`.
- `bind_socket` sets the socket to 0666 on purpose: any local user may connect,
  but only gets a credential for their own kernel-reported identity. The secret
  is the JWKS file, which must stay 0600. The loader (`HmacKeySet::from_path`)
  does not enforce that mode.

## Pitfalls

- `verify_user_rpc` checks only HMAC, `key_id`, time window, and `cluster_id`.
  Audience/epoch binding and replay (nonce) rejection happen in
  `verify_native_bearer` (`spur-core/src/auth.rs`). Any new verifier must go
  through `verify_native_bearer`, not call `verify_user_rpc` directly.
- `mint_blocking` exists because tonic interceptors cannot `.await`. Its timeout
  is mandatory: a mint that accepts but never replies would block the thread
  forever, and an async timeout around the RPC cannot preempt it.
- JWKS is loaded once at startup. Key rotation requires restarting `spurauthd`.
