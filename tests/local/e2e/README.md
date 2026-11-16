# Local daemon E2E tests

These tests start the real controller, agent, supervisor, and CLI in disposable
Linux user, mount, PID, and network namespaces. Their `/proc` is a real procfs containing
only the fixture's processes. Temporary spool directories replace `/var/spool`
inside the mount namespace; a read-only cgroup inventory replaces
`/sys/fs/cgroup`. Private `/etc`, `/run`, and `/var/cache` hide host service
configuration, sockets, CDI specifications, and update cache. The network
namespace has only loopback, so daemon update checks cannot access the network.
No SSH or installed service is used.

Build the binaries, install `tests/requirements.txt`, and run:

```bash
cargo build --locked -p spurctld -p spurd -p spur-stepd -p spur-cli
SPUR_TEST_BINARIES_DIR="$PWD/target/debug" pytest -q tests/local/e2e
```

Tests skip when namespaces or binaries are unavailable. A local gRPC proxy
forwards real controller calls and provides explicit query failures and barriers.
File waits use inotify events instead of assumed startup delays. A stopped
namespace kills all its remaining supervisors and workloads.

The suite is included in the default pytest test paths. The CI deployment jobs
still select the native-host and Kubernetes suites explicitly; they do not run
these local tests.

The deep-spool regression uses a 2 MiB Rust worker stack and an explicit
65536 soft/hard file-descriptor limit. It skips if the inherited hard limit
cannot accommodate that setting. Startup must finish without a crash; traversal
limits may safely retain the deep spool.

A separate single-threaded launcher denies `openat2` with `ENOSYS` using seccomp
before executing the agent. The completion regression runs a real job and checks
that its spool is removed without following a nested symlink. The filter applies
only to that agent and its children. This case skips if native x86_64/aarch64 or
seccomp filtering is unavailable.

Startup tests check terminal/absent spool removal, nonterminal retention,
controller failures and legacy visibility/terminal responses, safe filesystem
traversal, real process owners, deletion retry, and cleanup ordering before the
agent serves.
Active workload tests launch with cgroups disabled and enable them for the
restarted agent, so supervisor adoption and output continuity do not require
native cgroup privileges. Populated cgroups and bind-mount deletion protection
are exercised by Rust fixtures (the mount fixture requires namespace support).
These local tests do not verify startup cleanup with native cgroup enforcement;
the existing native-host suite has no startup-spool assertions.
