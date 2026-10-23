# AGENTS.md -- spurauthd

## Overview

`spurauthd` is the Spur native credential mint daemon. It runs on every login
node and compute node, issuing short-lived HMAC-signed credentials that CLI
tools (`spurctl`) and agents (`spurd`) present in gRPC metadata to authenticate
user RPCs against controllers (`spurctld`).

The daemon itself never verifies credentials -- `spurctld` and `spurd` do that.
`spurauthd` only mints.

## Binary

```
spurauthd --cluster <name> [--socket <path>] [--jwks <path>] [--lifetime-secs <n>]
```

- `--cluster` (required): cluster name, used for socket path and embedded in credentials.
- `--socket`: override listen path. Default: `/run/spur/<cluster>/auth.sock`.
  Env `SPUR_AUTH_SOCKET` also overrides.
- `--jwks`: HMAC key set file. Default: `/etc/spur/auth.jwks`.
- `--lifetime-secs`: credential TTL. Default: 30 seconds.

Tracing level via `RUST_LOG` env (default `info`).

## Auth plugin system

Spur supports three auth plugins configured via `[auth] plugin` in
`slurm.conf` (or `$SPUR_AUTH_PLUGIN`):

| Plugin   | Mechanism                                    |
|----------|----------------------------------------------|
| `spur`   | Native HMAC credentials via `spurauthd`      |
| `jwt`    | Standard JWT tokens (Slurm-compatible)       |
| `none`   | Auth disabled                                |

`munge` is recognized in config but **not implemented** -- the parser rejects
it at startup with an explicit error.

## Token management

### Minting flow

1. Client connects to the Unix socket.
2. Server reads `SO_PEERCRED` from the kernel -- the caller's UID/GID/PID.
   The caller cannot forge this; the kernel populates it.
3. UID is resolved to a username via NSS (`getpwuid`). The name is baked into
   the credential so remote verifiers skip NSS lookup.
4. A `UserRpcCredential` is assembled: cluster_id, issuer_host, audience,
   audience_epoch, user, uid, gid, issued_at, expires_at, 16-byte random
   nonce, and key_id.
5. The credential is serialized to canonical big-endian signing bytes (not
   JSON/JWT -- a fixed-field binary format with magic `SPURCRD1`).
6. Signing bytes are HMAC-SHA256'd with the default key from `auth.jwks`.
7. The signed token (`SPURTKN1` envelope: key_id + payload + signature) is
   base64url-encoded and returned to the client.

### Credential properties

- **Short-lived**: default 30s TTL. Clock skew tolerance: 5s.
- **One-shot**: each credential carries a unique 16-byte nonce.
- **Audience-bound**: credentials name a specific verifier (e.g. `spurctld-1`)
  and epoch. Verifiers reject credentials not addressed to them.
- **Cluster-scoped**: `cluster_id` is authenticated in the signed payload.
- **Domain-separated**: the `CredentialKind` byte (UserRpc=1, Job=2, Step=3,
  ControllerRpc=4, Node=5, Forwarded=6) is signed immediately after the
  version, preventing cross-kind forgery.

### Verification (performed by spurctld/spurd, not spurauthd)

`verify_user_rpc()` in `native_mint.rs`:
1. Base64url-decode the token.
2. Look up the `key_id` in the HMAC key set.
3. Verify the HMAC-SHA256 signature.
4. Deserialize the `UserRpcCredential` from signing bytes.
5. Check `key_id` consistency, time window (issued_at/expires_at + skew),
   and cluster_id match.

## JWKS key management

Key material lives in `/etc/spur/auth.jwks` (mode 0600). The file is standard
JWKS with `HS256`/`oct` keys. Each key has:

- `kid`: unique identifier.
- `use`: `"default"` for the active minting key. Exactly one required.
- `not_before` / `exp`: optional activation window.
- `k`: base64url-encoded secret (minimum 32 bytes).

Key rotation: add a new default key, keep the old key without `"use":"default"`
for overlap verification, then remove it after its `exp`. Daemons load JWKS
once at startup; swap the file and restart to pick up changes.

## Unix-socket interface

### Wire protocol

Length-prefixed frames over a Unix stream socket. Max frame: 8192 bytes.

```
Frame: [4-byte big-endian length][body]
```

**Request** (client -> server):
```
[8B magic "SPURMNT1"][2B version=1][1B op=1][4B+audience_str][8B epoch]
```

**Response** (server -> client):
```
[8B magic "SPURMNT1"][2B version=1][1B status][4B+token_or_error_str]
```

Status: 0 = OK (token follows), 1 = error (message follows).

### Socket permissions

The socket is bound mode 0666 (world-connectable). Security relies on
`SO_PEERCRED` -- any local user can request a credential, but only for their
own identity. The JWKS file (the secret) stays mode 0600.

### Client APIs

- `mint()`: async client, used in tokio contexts.
- `mint_blocking()`: sync client with explicit timeout, used in tonic gRPC
  interceptors that cannot `.await`. Timeout prevents indefinite hang if the
  mint accepts but never replies.

## Credential types (native_cred.rs)

| Kind            | Signed by     | Purpose                              |
|-----------------|---------------|--------------------------------------|
| UserRpc         | HMAC (mint)   | CLI/user -> controller RPCs          |
| Job             | Ed25519 (ctl) | Controller-signed job launch proof   |
| Step            | Ed25519 (ctl) | Controller-signed step launch proof  |
| ControllerRpc   | Ed25519 (ctl) | Controller -> agent service calls    |
| Node            | Ed25519 (node)| Node registration/heartbeat identity |
| Forwarded       | Ed25519 (ctl) | Controller-to-controller forwarding  |

Only `UserRpc` credentials flow through `spurauthd`. The rest are minted
directly by controllers or nodes using Ed25519 key sets.

## Source layout

```
crates/spurauthd/src/main.rs     -- CLI entry point, arg parsing, server bootstrap
crates/spur-core/src/native_mint.rs  -- CredentialMint, serve(), mint(), verify_user_rpc()
crates/spur-core/src/native_jwks.rs  -- HmacKeySet, Ed25519 key sets, JWKS parsing
crates/spur-core/src/native_cred.rs  -- Credential types, binary serialization, validation
crates/spur-core/src/auth.rs         -- Auth plugin dispatch, Identity binding
```

## Dependencies

Runtime: `spur-core`, `tokio`, `clap`, `anyhow`, `tracing`.
Crypto (via spur-core): `aws-lc-rs` (HMAC-SHA256, Ed25519), `rand`, `base64`.
Unix: `nix` (SO_PEERCRED, getpwuid), `whoami` (hostname).
