Native credential mint
======================

The credential mint issues short-lived native user-RPC credentials from kernel
peer credentials on a Unix-domain socket. It is not a JWT issuer.

Run ``spurauthd`` on every host that mints: login nodes (CLI) and compute
nodes (``spurd`` register/heartbeat/completion). ``spurctld`` and ``spurd``
verify credentials; they do not embed this server. Without a mint socket,
``auth.mode = "required"`` rejects those RPCs.

Socket path
-----------

Default::

   /run/spur/<cluster-name>/auth.sock

Override with ``--socket`` or ``$SPUR_AUTH_SOCKET``. The socket is mode
``0666`` so ordinary users can connect. Signing keys stay in
``/etc/spur/auth.jwks`` (mode ``0600``) and are never readable through the
socket.

The mint takes UID/GID/PID from ``SO_PEERCRED``, resolves the username with
NSS on that host, and signs a one-shot credential. Callers cannot supply a
username, UID, or role.

Example::

   spurauthd --cluster cluster-a --jwks /etc/spur/auth.jwks

CLI
---

Set ``[auth] plugin = "spur"`` (or ``$SPUR_AUTH_PLUGIN=spur``). Every CLI user
RPC, including stream opens and retries, asks the mint for a new credential.
The CLI first calls unauthenticated ``Ping`` on the controller or agent and
mints against the advertised audience (``spur/<cluster>/{controller|agent}/<hostname>``)
and boot epoch. A verifier restart chooses a new random epoch, so in-flight
credentials stop verifying.

Agents use the same mint for controller RPCs (register, heartbeat, completion)
after Ping. Each compute host therefore needs ``spurauthd`` (or
``$SPUR_AUTH_SOCKET`` pointing at one). Without that, ``auth.mode =
"required"`` rejects agent calls before join or node tokens in the body are
read.

Verifier
--------

``spurctld`` and ``spurd`` load ``/etc/spur/auth.jwks`` when
``[auth] plugin = "spur"``. They verify HMAC, ``kind = user-rpc``,
``cluster_id``, the validity window, the instance audience, and the boot epoch.
A nonce is recorded in a bounded replay cache until expiry plus clock skew;
the same credential cannot be accepted twice at that audience. Username, UID,
and GID come from the signed credential; the verifier does not call
``getpwuid``. JWT user tokens are rejected on this plugin. gRPC and REST on
the same controller share one verifier and replay cache. gRPC ``Ping`` does
not require a credential and advertises the audience and epoch so a client
can mint. REST ``/ping`` is liveness only and does not return those fields.

Without a config file (for example the k8s operator when ``--config`` is
absent), set ``$SPUR_AUTH_PLUGIN=spur`` and ``$SPUR_CLUSTER_NAME`` to the same
cluster name the controller uses. ``$SPUR_CLUSTER_NAME`` is required: the
agent audience is ``spur/<cluster>/agent/<hostname>``, and a missing name is
refused at startup rather than defaulted.

Controller and node identity
----------------------------

``plugin = "spur"`` uses separate Ed25519 JWKS files from the HMAC user set.
Controllers load private signing material; agents load verification material
only and cannot mint these tokens.

* ``/etc/spur/controller-signing.jwks`` (``$SPUR_CONTROLLER_SIGNING_JWKS``) —
  Raft peer identity envelopes and controller-to-agent service credentials.
* ``/etc/spur/controller-verification.jwks`` (``$SPUR_CONTROLLER_VERIFICATION_JWKS``) —
  agents verify controller-to-agent credentials. There is no fallback to
  ``auth.jwks`` or to job-credential keys.
* ``/etc/spur/node-signing.jwks`` (``$SPUR_NODE_SIGNING_JWKS``) —
  controller-issued node identity at token admission. Distinct from the
  controller-to-agent key.

The controller Pings the agent (no credential) and mints a controller-to-agent
token for that Ping's audience and boot epoch — the same binding user RPCs
use. Agents reject a token whose audience or epoch is not their own, so a
token captured on one node cannot be replayed on another.

A Raft follower authenticates the user once, consumes the user nonce, and
forwards a signed identity envelope (``x-spur-identity`` / ``x-spur-forwarded``)
instead of the original user credential.

Job and step credentials
------------------------

* ``/etc/spur/cred-signing.jwks`` (``$SPUR_CRED_SIGNING_JWKS``) on controllers.
* ``/etc/spur/cred-verification.jwks`` (``$SPUR_CRED_VERIFICATION_JWKS``) on agents.

The controller signs a job execution credential only after Raft has committed
the allocation, then attaches it to every ``LaunchJob`` for that run. A step
credential is signed after ``create_step`` commits and returned on
``CreateJobStepResponse``. ``spurd`` verifies node, resource slice, Unix
identity, command digest, and run attempt, and records the credential for
idempotency. A later cancel of that attempt refuses a replay of the same
credential. When ``plugin = "jwt"``, these fields stay empty and are not
required.

Key generation and rotation
---------------------------

Generate files as root on a controller host. Signing documents are mode
``0600``; copy the verification document to every agent.

::

   spur auth-keys hmac --kid auth-1 --out /etc/spur/auth.jwks
   spur auth-keys ed25519 --kid cred-1 \
     --signing /etc/spur/cred-signing.jwks \
     --verify /etc/spur/cred-verification.jwks
   spur auth-keys ed25519 --kid ctrl-1 \
     --signing /etc/spur/controller-signing.jwks \
     --verify /etc/spur/controller-verification.jwks
   spur auth-keys ed25519 --kid node-1 \
     --signing /etc/spur/node-signing.jwks \
     --verify /tmp/node-verification.jwks

To rotate, add a new default key to the JWKS (keep the previous key until
in-flight credentials expire), distribute verification files, then remove the
old key. ``spurctld`` and ``spurd`` read JWKS at process start; replace the
files and restart to pick up a new set.

Audit and metrics
-----------------

Native verify success/failure, nonce replay, execution-credential verify, and
RBAC denials are process-wide counters exported on the controller at
``/metrics/auth`` (OpenMetrics). Denials also emit a structured warning
(``rbac denied: cluster administrator required``).

