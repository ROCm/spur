# Spur Authentication and Authorization Model

Setup lives in user docs: `[auth]` plugins, modes, and key distribution in
`docs/admin-guide/configuration.rst`; roles and their sources in
`docs/admin-guide/rbac.rst`; mint socket, audiences, Raft forwarding, key
generation, and rotation in `docs/developer/native-credential-mint.rst`. This
page covers trust boundaries and the invariants behind them.

## 1. Plugins

- `plugin = "spur"`: native per-RPC credentials minted by `spurauthd`. Only
  native credentials verify; JWT user tokens are rejected even if a `jwt_key`
  is configured.
- `plugin = "jwt"`: HS256 bearer tokens signed with `auth.jwt_key`. A native
  credential (anything that decodes as a `SignedToken`) is explicitly rejected
  with a message that the plugin is not `"spur"`.
- `plugin = "none"`: no credential; the server trusts the asserted username.
  `none` with `mode = "required"` is rejected at config validation, since no
  credential could ever verify.
- `munge` is recognized but rejected at startup, never silently ignored.
- `SPUR_AUTH_PLUGIN` overrides the config file for `spur-cli`. For `spurd`
  (its outbound controller calls) and `spur-k8s` it applies only when no config
  file is present. `spurctld` reads the config file only.

## 2. Credentials and who can mint them

Each native credential carries a `CredentialKind` byte inside the signed
payload, so a credential of one kind never verifies as another.

- **UserRpc** (HMAC-SHA256, `auth.jwks`): minted by `spurauthd` for any local
  user, from `SO_PEERCRED`. Short-lived (`DEFAULT_LIFETIME_SECS`), carries a
  random nonce (single use), and names one verifier audience plus its boot
  epoch, so it cannot be replayed to another controller or agent.
- **Job / Step** (Ed25519, `cred-signing.jwks`): minted by `spurctld` for a
  launch. Binds the allocation, command digest, and Unix identity; the agent
  verifies it before starting work (section 6).
- **ControllerRpc** (Ed25519, `controller-signing.jwks`): minted by `spurctld`
  per agent call, bound to the target agent's audience and boot epoch, with a
  nonce for replay protection.
- **Node** (Ed25519, `node-signing.jwks`): minted by `spurctld` at node token
  admission. Under `plugin = "jwt"`, node tokens are HS256 JWTs from `jwt_key`
  instead.
- **Forwarded** (Ed25519): minted by a controller that already authenticated a
  user RPC and forwards it to the Raft leader. Binds the caller's identity, the
  RPC path, and a SHA-256 digest of the request body.

HMAC keys are a shared secret: any holder can mint and verify. Ed25519 signing
keys stay on the controller; agents hold only verification sets, and a
verification JWKS containing a private key (`d`) is rejected at load so
signing material cannot leak to agents by mistake.

## 3. Credential flow (plugin = "spur")

```text
CLI / spurd             spurauthd                     spurctld / spurd (verifier)
   |-- Ping (unauthenticated) ----------------------------->|
   |<-- auth_audience, auth_epoch --------------------------|
   |-- connect auth.sock -->|                               |
   |                        |-- SO_PEERCRED uid/gid         |
   |                        |-- NSS getpwuid(uid)           |
   |                        |-- build UserRpc (cluster,     |
   |                        |   audience, epoch, nonce)     |
   |                        |-- HMAC sign                   |
   |<-- token --------------|                               |
   |-- RPC, Authorization: Bearer <token> ----------------->|
   |                                   verify_native_bearer():
   |                                   HMAC, key_id, cluster_id,
   |                                   time window, audience, epoch,
   |                                   replay cache
```

- `Ping` is exempt from authentication in both the controller and agent
  middleware; it is how a caller learns the audience and epoch to mint for.
- `spurd` uses the same flow for its controller calls: `AgentControllerInterceptor`
  calls `mint_blocking()` against the local `spurauthd` before every RPC.
- Under `plugin = "jwt"` the token (`spur token user` or an external issuer)
  carries `sub`, `uid`, `exp`, `iat`, and `admin` but no `gid`. The controller
  ignores the token's uid and resolves uid/gid from the username through NSS
  (`resolve_unix_credentials()`).

## 4. Verification layers

- **Controller** (`spurctld/src/auth_middleware.rs`): a Tower layer wrapping
  every service on the port, so the accounting service is behind the same gate
  as the controller. Only the auth layers insert an `Identity` (this
  middleware, and REST `dispatch` after `rest_auth` verifies), so a handler
  that finds one knows it was verified. It also inserts a `Verified`
  marker that separates verified callers from anonymous ones in audit logs.
  For forwarded RPCs it verifies the `ForwardedIdentity` envelope, the RPC path
  binding, and the body digest.
- **Agent** (`spurd/src/auth_middleware.rs`): a separate Tower layer on the
  agent port. Both layers call the shared ruling in `spur-core`
  (`spur_core::auth::authenticate_bearer`), so the daemons cannot drift apart on
  a security decision.
- **REST**: `rest_auth` uses the same `BearerAuth` ruling, so JWT and native
  credentials verify identically on gRPC and REST.

### Behavior by mode

The per-mode outcomes are in `docs/admin-guide/configuration.rst`
("Authentication modes"). Not stated there:

- Permissive tolerates the absence of a credential, never a bad one, or
  forging would beat sending none.
- A malformed `Authorization` header (no `Bearer` prefix, empty token) is
  rejected in every mode, including `disabled`, never downgraded to anonymous.

## 5. Authorization

- **`require_admin`** (`spurctld` `server.rs`): partition create/update/delete,
  node update/drain/remove, admission tokens, reconfigure. A verified caller
  must resolve to `Role::Administrator`; an anonymous caller passes because
  `caller_is_privileged` returns true when there is no identity. This keeps
  no-auth deployments working and makes the check inert for anonymous callers.
- **Operator bar** (`caller_is_operator()`): acting on other users' jobs
  (cancel, suspend, resume, priority override). Needs a verified identity whose
  role operates jobs; anonymous callers never pass.
- **Reservations** (`require_reservation_manager`): passes a verified admin or
  operator. Otherwise it resolves the request's `user` through NSS and allows
  root or `sudo`/`wheel` members. For an authenticated caller `user` is
  overwritten from the identity first; for an anonymous caller it is the
  client-supplied value, so `user = "root"` passes. This is a boundary only
  under `required`.
- **Agent**: controller-only RPCs (launch, run, cancel, suspend, kubeconfig,
  join token) require `Identity::is_controller()` (subject `CONTROLLER_SUBJECT`).
  A valid user credential authenticates but cannot drive them. An anonymous
  caller is not refused (`require_controller` returns ok with no identity).

Role sources and their precedence are in `docs/admin-guide/rbac.rst`.

## 6. Boundaries that hold in every mode

- A malformed `Authorization` header is rejected. Under `permissive` and
  `required`, invalid credentials are rejected and `UserRpc` / `ControllerRpc`
  credentials are checked for audience, epoch, and nonce replay (under
  `disabled` a well-formed token is ignored).
- `bind_job_spec` overwrites spec `user`/`uid`/`gid` from a verified identity.
  JWT identities re-resolve uid/gid through NSS. Native identities set
  `trusted_unix`, so the verifier uses the mint-host uid/gid and skips NSS
  re-resolution.
- `check_job_owner` has no bypass for an empty `user` or a literal `"root"`:
  only `is_internal`, derived from a verified identity, reaches jobs the caller
  does not own.
- Execution credentials: the agent checks the signature, `cluster_id`, time
  window, kind, that it is the named node, the run attempt, uid/gid, command
  digest, and resource slice before launching. `LaunchAcceptance` makes a
  launch idempotent per `(job_id, step_id, run_attempt)`: the exact same
  credential is accepted without relaunching, a different one for the same
  attempt is `IdempotencyConflict`, and a cancelled attempt stays rejected.
  A credential is recorded only after the request validates (see
  [job execution](../execution/job-execution.md#1-dispatch-and-admission)).
  Under `plugin = "spur"` (agent holds `cred_keys`), a compromised node cannot
  forge a launch on another node, since it holds no Ed25519 signing key.
  Without `cred_keys` the agent skips execution-credential verification.

## 7. What anonymous callers can do (`permissive`, `disabled`)

None of these applies under `mode = "required"`:

- `require_admin` is open: any peer that reaches the controller port can
  create partitions, manage nodes, issue tokens, and reconfigure the cluster.
- `bind_job_spec` leaves the spec as sent, so a client can submit as any
  user, uid, and gid.
- `read_scope` returns `ReadScope::All`, so `private_data` restricts only
  identified non-operator users.
- `check_job_caller` falls back to matching the claimed username, or a
  non-zero caller uid against the job's submit-time uid. This is Slurm-like
  same-session semantics resting on what the client reports.
- An anonymous `CancelJob` with an empty `user` is treated as the in-cluster
  daemon (`is_daemon_caller`) and may cancel any job.
- Reservation management trusts the claimed `user` (section 5).
- On agents, anyone who reaches the port can call controller-only RPCs; the
  agent logs a warning per request under `permissive`.

## 8. Key handling hazards

- **HMAC is a shared secret.** Any host holding `auth.jwks` can forge
  `UserRpc` credentials for any user, so a compromised node can impersonate
  anyone. This is the accepted HPC model (like Slurm's `munge.key`).
- **`jwt_key` has the same property.** Under `plugin = "jwt"` agents verify
  with `jwt_key`, so any holder can mint a token with the controller's subject
  and drive controller-only RPCs on other nodes.
- **Key set rules.** HMAC keys are at least 32 bytes. Exactly one key has
  `"use": "default"`; it mints and must be active (`not_before`, `exp`) at load.
  Keys without the marker verify only, which is what makes overlap rotation work.
- **Rotation.** Steps are in `docs/developer/native-credential-mint.rst`. JWKS
  files are read once at startup; no daemon calls `LiveSet::reload()`, so
  rotation needs a restart.
- **Clocks.** Every verification depends on synchronized clocks; skew beyond
  `CLOCK_SKEW_SECS` rejects credentials.
- **Replay cache is in memory.** A restart forgets seen nonces, but a restarted
  verifier picks a new random boot epoch, so earlier credentials fail the epoch
  check.
- **NSS on the controller.** JWT identities must resolve through NSS on the
  controller host; an unresolvable user fails the request.
