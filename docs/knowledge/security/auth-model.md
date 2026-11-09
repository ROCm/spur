# Spur Authentication and Authorization Model

## 1. Auth plugin system

The auth plugin is set in `spur.conf` under `[auth] plugin`. Three values are accepted:

| Plugin | Mechanism |
|--------|-----------|
| `spur` | Native per-RPC HMAC credentials via `spurauthd` Unix socket |
| `jwt`  | Standard HS256 JWT bearer tokens (Slurm-compatible) |
| `none` | No authentication; identity derived from the calling process's UNIX credentials |

`munge` is recognized by the config parser but rejected at startup with an explicit
error rather than being silently ignored. The environment variable
`SPUR_AUTH_PLUGIN` overrides the config file for the CLI (`spur-cli`). For
`spurd`/`spur-k8s`, it applies only when no config file is present. `spurctld`
reads the config file only.

When `plugin = "spur"`, native credentials are verified exclusively -- JWT user
tokens are rejected even if a `jwt_key` is configured. When `plugin = "jwt"`,
a native credential presented in the `Authorization` header is detected (by
successfully decoding it as a `SignedToken`) and explicitly rejected with a
message stating the plugin is not `"spur"`.

The combination `plugin = "none"` with `mode = "required"` is rejected at config
validation because no credential can ever be verified.

## 2. Credential types and lifecycle

Six credential kinds exist, distinguished by a `CredentialKind` byte encoded
immediately after the version in the signed payload. This domain-separation
prevents a credential of one kind from verifying as another.

| Kind (byte) | Struct | Signed by | Purpose |
|-------------|--------|-----------|---------|
| UserRpc (1) | `UserRpcCredential` | HMAC-SHA256 | CLI/user to controller or agent RPCs |
| Job (2) | `ExecutionCredential` | Ed25519 | Controller-signed job launch authority |
| Step (3) | `ExecutionCredential` | Ed25519 | Controller-signed step launch authority |
| ControllerRpc (4) | `ControllerRpcCredential` | Ed25519 | Controller to agent service identity |
| Node (5) | `NodeIdentityCredential` | Ed25519 | Node registration/heartbeat identity |
| Forwarded (6) | `ForwardedIdentity` | Ed25519 | Controller-to-controller identity forwarding |

### Lifecycle

**UserRpc**: minted by `spurauthd` on request from any local user. Default
TTL is 30 seconds. Each credential carries a 16-byte random nonce and is
single-use (replay cache). Audience-bound: names a specific verifier host
and boot epoch, so a credential for one controller cannot be replayed to
another.

**Job/Step**: minted by `spurctld` when launching work. Job credentials
default to 24-hour TTL; step credentials default to 1 hour. Contain the
full resource allocation (nodes, CPUs, memory, devices), command digest,
container digest, uid/gid, supplementary gids, account, partition, and QoS.
The agent verifies these claims before starting work.

**ControllerRpc**: minted by `spurctld` for each RPC it sends to an agent.
Audience-bound to the target agent's hostname and boot epoch. Carries a
nonce for replay protection.

**Node**: minted by `spurctld` at node token admission, signed with the
node-signing key set. Under `plugin = "jwt"`, node tokens use HMAC JWT from
`jwt_key` instead.

**Forwarded**: minted by a controller peer that has already authenticated
a user RPC and needs to forward it to the Raft leader. Binds the original
caller's identity, the forwarding action (RPC path), and a SHA-256 digest
of the request body.

## 3. HMAC vs Ed25519 -- which signs what

Two cryptographic primitives are used, with strict separation:

**HMAC-SHA256** (`auth.jwks`, symmetric `HS256`/`oct` keys):

- Signs `UserRpc` credentials. The JWKS file is a shared secret: any host
  holding it can both mint and verify. This is the HPC trust model (like
  Slurm's `munge.key`).
- Key material minimum: 32 bytes.
- Default key marked with `"use": "default"`. Overlap keys (without the
  `"default"` marker) verify old credentials during rotation but do not mint.

**Ed25519** (asymmetric `EdDSA`/`OKP`/`Ed25519` keys):

- Signs `Job`, `Step`, `ControllerRpc`, `Node`, and `Forwarded` credentials.
- Signing keys (private `d` + public `x`): held by the controller in
  `/etc/spur/cred-signing.jwks` and `/etc/spur/controller-signing.jwks`.
- Verification keys (public `x` only): distributed to agents in
  `/etc/spur/cred-verification.jwks` and `/etc/spur/controller-verification.jwks`.
- The verification JWKS explicitly rejects documents containing a private key
  (`d` field) to prevent accidental key leakage to agents.
- Only the controller can mint; agents can verify but never sign.

### JWKS file paths

| File | Content | Holder |
|------|---------|--------|
| `/etc/spur/auth.jwks` | HMAC-SHA256 shared secret | All nodes running `spurauthd` |
| `/etc/spur/cred-signing.jwks` | Ed25519 private keys for job/step credentials | Controller only |
| `/etc/spur/cred-verification.jwks` | Ed25519 public keys for job/step credentials | Agents |
| `/etc/spur/controller-signing.jwks` | Ed25519 private keys for controller-to-agent RPCs | Controller only |
| `/etc/spur/controller-verification.jwks` | Ed25519 public keys for controller identity | Agents |
| `/etc/spur/node-signing.jwks` | Ed25519 private keys for node identity tokens | Controller only |

Environment overrides: `SPUR_AUTH_JWKS`, `SPUR_CRED_VERIFICATION_JWKS`,
`SPUR_CONTROLLER_VERIFICATION_JWKS`.

## 4. Token flow: mint, present, verify

### User RPC flow (plugin = "spur")

```text
User CLI                spurauthd              spurctld
   |                       |                      |
   |-- connect(Unix) ----->|                      |
   |                       |-- SO_PEERCRED -----  |
   |                       |   (kernel uid/gid)   |
   |                       |-- NSS getpwuid(uid)   |
   |                       |-- build UserRpcCred   |
   |                       |   (cluster, audience, |
   |                       |    epoch, nonce, ...)  |
   |                       |-- HMAC-SHA256 sign    |
   |<-- base64url token ---|                      |
   |                       |                      |
   |-- gRPC: Authorization: Bearer <token> ------>|
   |                                              |
   |                           verify_user_rpc(): |
   |                           1. base64url decode |
   |                           2. HMAC verify      |
   |                           3. deserialize cred |
   |                           4. check cluster_id |
   |                           5. check audience   |
   |                           6. check epoch      |
   |                           7. check time window|
   |                           8. replay cache     |
   |                                              |
   |<------------ gRPC response ------------------|
```

The audience and epoch are discovered via an unauthenticated `Ping` RPC before
the first authenticated call. `Ping` is exempted from authentication in both
the controller and agent middleware.

### Agent-to-controller flow (plugin = "spur")

`spurd` uses the same minting flow. On startup, it calls `Ping` on the
controller to discover `auth_audience` and `auth_epoch`. A tonic interceptor
(`AgentControllerInterceptor`) calls `mint_blocking()` against the local
`spurauthd` socket before every RPC, obtaining a fresh `UserRpc` credential
addressed to the controller.

### JWT flow (plugin = "jwt")

The user obtains a JWT via `spur token user` or an external issuer. The token
contains `sub` (username), `uid`, `exp`, `iat`, and `admin` claims, signed
with HS256 using `auth.jwt_key`. The controller verifies via `jsonwebtoken`
library. JWT tokens carry no `gid`; the controller resolves uid/gid from NSS
on its own host via `resolve_unix_credentials()`.

## 5. Middleware: authentication layer (gRPC + REST)

### Controller middleware (`spurctld/src/auth_middleware.rs`)

A Tower layer wrapping every service on the controller's port. This is the
single verification point for the control plane -- it covers both the
controller gRPC service and the accounting service (which has no authorization
of its own).

On success, a verified `Identity` is inserted into the request extensions.
Only this layer inserts an `Identity`, so a handler that finds one knows it
was verified here. A `Verified` marker is also inserted to distinguish
credential-verified identities from anonymous ones in audit logs.

The middleware additionally handles forwarded RPCs from Raft followers: it
verifies the signed `ForwardedIdentity` envelope, checks the RPC path binding,
and validates the SHA-256 digest of the request body against the envelope's
`request_digest`.

### Agent middleware (`spurd/src/auth_middleware.rs`)

A separate Tower layer on the agent's gRPC port. Structurally identical to
the controller middleware but with a different hint message. The ruling
function is the shared `authenticate_bearer_inner()` in `spur-core`, so both
daemons cannot drift apart on security decisions.

### REST surface

The REST gateway wraps the gRPC client with the same `BearerAuth` configuration.
JWT and native credentials are verified identically via the shared
`authenticate_bearer` function.

### Behavior by mode

| Mode | No credential | Valid credential | Invalid credential |
|------|--------------|-----------------|-------------------|
| `required` | Reject | Authenticate | Reject |
| `permissive` | Anonymous (log warning) | Authenticate | Reject |
| `disabled` | Anonymous | Anonymous (ignored) | Anonymous (ignored) |

A malformed `Authorization` header (missing `Bearer` prefix, empty token) is
always rejected regardless of mode. This prevents silent downgrade to anonymous.

## 6. Handler authorization

Authorization is a two-tier system resolved after authentication:

### Tier 1: `require_admin`

Used for cluster-tenancy mutations: create/update/delete partitions, node
placement, token management, reconfigure. The check:

- If the caller has no verified identity (anonymous under permissive/disabled),
  access is granted. This preserves backward compatibility with no-auth deployments.
- If the caller has a verified identity, they must resolve to `Role::Administrator`.

This means `require_admin` is effectively inert under `permissive` mode for
unauthenticated callers.

### Tier 2: Operator bar

Used for job operations that affect other users' work: cancel/suspend/resume
jobs owned by others, priority override, etc. Resolved via `caller_is_operator()`,
which checks:

- The accounting database's `admin_level` for the user (Operator or Administrator).
- The RBAC role resolved from `[auth] operator_groups` NSS membership.
- `is_admin` flag from the credential.

### Role hierarchy

Four fixed roles, resolved in `rbac.rs`:

| Role | Level | Capabilities |
|------|-------|-------------|
| `User` | 1 | Own jobs only |
| `Coordinator` | 2 | Own jobs + visibility into coordinated accounts |
| `Operator` | 3 | All job operations (`operates_jobs()`) |
| `Administrator` | 4 | Cluster mutations (`administers_cluster()`) |

Role resolution sources (highest wins):

1. `Identity.is_admin` flag from the credential
2. `[auth] cluster_admins` name list
3. `[auth] admin_groups` NSS group membership
4. `allow_uid_zero_administrator` (opt-in, requires `trusted_unix`)
5. Accounting database `admin_level` (fails closed if cache not loaded)
6. `[auth] operator_groups` NSS group membership

### Reservation management

Uses a stricter check (`require_reservation_manager`) that does not fall
through for anonymous callers. Checks `caller_is_admin`, `caller_is_operator`,
or whether the user belongs to `sudo`/`wheel` via NSS group lookup.

### Agent-side authorization

The agent gates controller-only RPCs (job launch, cancel) on
`Identity::is_controller()`, which checks `id.user == "spurctld"`. A valid
*user* token authenticates but cannot drive controller-only RPCs.

## 7. Permissive mode behavior

`permissive` is the default mode, designed as a migration path from no-auth
to full auth.

Behavior:

- A missing credential is accepted as `BearerOutcome::Anonymous`. The request
  proceeds with no `Identity` in extensions. A warning is logged naming the
  RPC path.
- A valid credential is verified and the `Identity` is attached normally.
- An invalid credential is always rejected. "Permissive tolerates the
  *absence* of a credential, never a bad one, or forging would beat sending
  none."

Consequences for authorization:

- `require_admin`: an anonymous caller (no `Identity`) passes because
  `caller_is_privileged` returns `true` when `identity.is_none()`. This
  means any unauthenticated caller can perform admin operations.
- `caller_is_operator`: returns `false` for anonymous callers, so
  operator-level job operations on other users' work require a credential.
- `PrivateData` scoping: anonymous callers get `ReadScope::All` (unrestricted
  view), so `private_data` categories only restrict identified non-operator
  users.
- Job spec binding (`bind_job_spec`): unauthenticated callers are left as-is,
  meaning the client-supplied `user`/`uid`/`gid` is trusted.

## 8. JWKS key management and rotation

### File format

Standard JWKS (`{"keys": [...]}`) with extensions:

```json
{
  "keys": [{
    "alg": "HS256",
    "kty": "oct",
    "kid": "2026-11",
    "use": "default",
    "k": "<base64url-encoded-secret>",
    "not_before": 1730000000,
    "exp": 1735000000
  }]
}
```

Constraints enforced by the loader:

- Maximum 64 keys per file.
- Maximum file size: 1 MB.
- Exactly one key must have `"use": "default"`.
- HMAC keys: minimum 32 bytes.
- Ed25519 keys: exactly 32 bytes.
- No duplicate `kid` values.
- No empty `kid` values.
- `"use"` accepts only `"default"` or absent. Any other value is rejected.
- The default key must be active at load time (`not_before` <= now < `exp`).
- Verification key sets reject documents containing private key material (`d` field).

### Rotation procedure

1. Add the new key with `"use": "default"` to the JWKS file.
2. Move the old key's `"use"` to absent (or remove it entirely). Keep the old
   key in the file for overlap verification.
3. Set `"exp"` on the old key to the desired retirement time.
4. Restart the daemon. JWKS files are loaded once at startup; there is no
   hot-reload. `LiveSet::reload()` exists for atomic swap but requires the
   caller to invoke it.
5. After the old key's `exp` passes, remove it from the file.

### Key generation

`generate_hmac_jwks(kid)` produces a single-key HMAC JWKS with 32 random bytes.
`generate_ed25519_jwks(kid)` produces a matched pair: a signing JWKS (with `d`
and `x`) and a verification JWKS (with `x` only).

## 9. Execution credentials (Ed25519, job launch proof)

Execution credentials prove the controller authorized specific work on specific
nodes with specific resources. They are the mechanism that prevents a user from
bypassing the scheduler and directly asking an agent to run arbitrary work.

### Contents

An `ExecutionCredential` binds:

- `kind`: Job or Step
- `cluster_id`: must match the agent's cluster
- `job_id`, `step_id`, `run_attempt`
- `user`, `uid`, `gid`, `supplementary_gids`
- `account`, `partition`, `qos`
- `resources_by_node`: per-node CPU count, memory, and device allocations
- `command_digest`: SHA-256 of (script + argv + container_image)
- `container_digest`: optional SHA-256 of the container image name
- `issued_at`, `not_before`, `expires_at`
- `credential_id`: 16-byte random identifier for idempotency

### Signing (`sign_execution`)

The controller fills in `credential_id` (random), `key_id` (from the
default signing key), timestamps, and serializes the credential to canonical
signing bytes (big-endian, sorted nodes and devices). The Ed25519 signature
is computed over these bytes.

### Verification (`verify_execution`)

The agent:

1. Decodes the base64url `SignedToken` envelope.
2. Verifies the Ed25519 signature using the public verification key set.
3. Deserializes the `ExecutionCredential` from signing bytes.
4. Checks `key_id` consistency, `cluster_id`, and time window.
5. Calls `require_node(hostname)` to ensure this agent is named.
6. Calls `require_run_attempt` to match the expected attempt.
7. Calls `require_unix(uid, gid)` for identity consistency.
8. Calls `require_slice(hostname, cpus, memory, devices)` to validate
   the resource allocation matches what this agent was told to set up.
9. Calls `require_command_digest` to verify the script/argv/container
   have not been tampered with.

### Idempotency (`LaunchAcceptance`)

The agent maintains a per-process cache of accepted launches keyed by
`(job_id, step_id, run_attempt)`. An exact duplicate credential (same
`credential_id` and `command_digest`) is accepted but does not re-launch.
A different credential for the same attempt is rejected as
`IdempotencyConflict`. A cancelled attempt stays rejected. The cache
evicts expired entries and, at capacity (default 8192), evicts the
entry with the earliest expiry.

## 10. Security boundaries and known gaps

### Boundaries that hold in all modes

- Invalid credentials are always rejected (even in `permissive`).
- Malformed `Authorization` headers are always rejected (never downgraded
  to anonymous).
- `disabled` mode does not verify even a valid token, ensuring it cannot
  be silently stricter than it claims.
- Domain separation: a `UserRpc` credential cannot verify as a `Job` or
  `ControllerRpc` credential and vice versa.
- Replay protection: `UserRpc` and `ControllerRpc` credentials carry a
  nonce checked against a replay cache.
- Audience binding: credentials name a specific verifier and boot epoch.
  A credential minted for one controller cannot be used on another.
- Job spec binding: `bind_job_spec` overwrites `user`/`uid`/`gid` from
  the verified identity, preventing privilege escalation via forged
  wire values. JWT identities re-resolve uid/gid through NSS.
- Execution credentials: the agent verifies the full resource allocation,
  command digest, and identity before launching work.
- Empty-user and literal-"root" string bypasses are eliminated: only an
  explicitly internal caller (`is_internal = true`) may access jobs with
  no owner. The `user` string from the wire is never trusted for privilege.

### Boundaries that only hold under `mode = "required"`

- **`require_admin` is porous in permissive mode.** Anonymous callers pass
  `caller_is_privileged` because `identity.is_none()` returns true. Any
  network peer that can reach the controller port can create partitions,
  manage nodes, issue tokens, and reconfigure the cluster.

- **Job spec binding is skipped for unauthenticated callers.** In
  `bind_job_spec`, when `identity` is `None`, the spec is left as-is.
  A client can submit a job as any user.

- **`PrivateData` scoping is unrestricted for anonymous callers.** The
  `read_scope` function returns `ReadScope::All` when there is no identity.
  Multi-tenant data isolation requires `required` mode.

- **Reservation management** has a stricter check
  (`require_reservation_manager`) that does not wave anonymous callers
  through, making it one of the few admin operations that restricts
  access even in permissive mode.

### HMAC shared-secret limitation

The HMAC key (`auth.jwks`) is a symmetric shared secret. Any host holding
it can both mint and verify credentials. A compromised compute node can
forge `UserRpc` credentials for any user. This is the accepted HPC trust
model (equivalent to Slurm's `munge.key`).

Execution credentials (`Job`, `Step`, `ControllerRpc`) use asymmetric
Ed25519, so agents can verify but not forge these. This is a meaningful
improvement: a compromised node cannot forge a job launch on another node.

### Other considerations

- **Clock dependency.** All credential verification depends on synchronized
  clocks. The skew tolerance is 5 seconds (`CLOCK_SKEW_SECS`). A clock
  difference larger than this causes credential rejection.

- **No JWKS hot-reload.** JWKS files are loaded once at startup. Key
  rotation requires daemon restart. The `LiveSet::reload()` API exists
  but is not wired to a file watcher or signal handler.

- **NSS resolution on controller.** JWT-based identities resolve uid/gid
  through NSS on the controller host. If the controller cannot resolve a
  username (no passwd entry), the request fails. Native (`spur` plugin)
  identities carry mint-host uid/gid and skip NSS re-resolution
  (`trusted_unix = true`).

- **`check_job_caller` uid fallback.** When unauthenticated (no `Identity`),
  the ownership check falls back to matching the caller's username or UID
  against the job's submit-time values. This provides Slurm-like
  same-session semantics but relies on the caller honestly reporting
  their identity.

- **Replay cache is per-process.** The `ReplayCache` is an in-memory
  data structure. If the controller restarts, previously seen nonces are
  forgotten. The short credential TTL (30 seconds) limits the window.
