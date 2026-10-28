# Simulated GPU discovery and scheduling

These tests run the real controller, agent, discovery code, and CLI against
synthetic driver files. They do not execute GPU kernels or measure fabric
performance. Use a disposable deployment with no host GPU device mounts.

Generate each node's fixture before starting its agent:

```bash
python3 tests/simulation/gpu_fixture.py /temporary/node0/sys --node 0 --grouped
```

Repeat for nodes 1–3. Run containers with `--init` so orphaned job processes
are reaped. Mount each generated directory read-only as `/sys` inside
its agent container, including the empty `fs/cgroup` directory. Configure nodes
`sim-node-[0-3]`, automatic device discovery, block topology, a one-second
scheduler interval, and short inventory refresh. Use four CPUs and at least
2048 MiB of schedulable memory per node. GPU/cgroup isolation must be disabled
in this hardware-free deployment. Run the adapter's CLI processes from a
writable `/state` working directory;
`spur alloc` inherits it, while the batch and standalone tests pass `--chdir`.
The grouped fixture has four GPUs per node, two NUMA nodes and two PCI roots,
with VRAM capacities of 24/192/192/288 GiB.
Nodes 0–1 form vPod 15; nodes 2–3 form vPod 16 in the same physical domain.
Without `--grouped`, the fixture has four separate roots, unknown NUMA, and
288 GiB per GPU. Its numeric device identity exercises generic AMD model naming.

Set `SPUR_SIM_CONTROL` to an executable adapter for your isolated deployment:

```bash
SPUR_SIM_CONTROL=/path/to/adapter pytest -q tests/simulation
```

Tests skip if the adapter is unset. The adapter protocol is:

| Operation | Required behavior |
| --- | --- |
| `cli <args...>` | Execute the real `spur` CLI; preserve stdout, exit status, and allocation-shell stdin. |
| `admin <args...>` | Execute the CLI with the isolated cluster administrator identity for creating/deleting the test reservation. The fixture user is `sim`. |
| `inventory` | Return JSON mapping node names to the GPU records asserted in `test_all_reported_gpu_properties`. Obtain values from controller inspection, not the generator's expected manifest. |
| `step <job-id> <args...>` | Execute `spur run` inside that existing allocation. A remote harness may set `SPUR_JOB_ID` for this operation; ordinary submission does not require it. |
| `mutate <node> <field> <value>` | Change only the disposable fixture: `type` changes KFD/PCI device IDs, `vram` changes the KFD bank, `numa` changes PCI NUMA, `root` changes the canonical host bridge and symlinks; other fields name `ualink` attributes. `<missing>` removes the attribute. |
| `hide-fabric` | With no active jobs, hide generated `ualink` directories and restart the simulated agents to establish a fresh non-discovered fleet for declaration-only tests. Leave KFD/PCI GPU facts intact. Removing a previously observed source from a running agent is discovery loss and must remain unavailable. |
| `reset` | Restore all grouped fixtures while preserving the mounted root and cgroup directory inodes. Waits in tests observe the controller's refreshed inventory. |
| `block-file enable` | Install `BlockName=a Nodes=sim-node-[0-1]` and `BlockName=b Nodes=sim-node-[2-3]` on controller and agents, configure `topology.external.block_file`, and reload/restart as required. Wait for fresh agent inventories before reconfiguring the controller; check the reload result, ignored-declaration notices, and revisions on file-only agents. Native blocks retain native bindings and need no file revision. |
| `block-file disable` | Restore automatic block discovery on controller and agents. |

The operator owns deployment, transport, and final container/network cleanup.
Tests cancel their submitted jobs during teardown. Preserve adapter command
logs and pytest results to link assertions to actual scheduling commands.
Fixture manifests contain expected properties plus every generated file hash
and relative symlink target. The fixture includes KFD identities, memory banks,
peer links, PCI ancestry, DRM symlinks, and read-only fabric membership attributes.
