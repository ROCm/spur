# GPU node prolog and epilog

Example compute-node hooks for a shared GPU node. Prolog checks the
allocation and refuses the launch when leftover GPU processes or containers
are still present. Epilog removes only resources owned by the job that is
ending, so several jobs can share one node.

Copy the scripts to a root-owned directory such as `/opt/spur/hooks` and make
them mode `0755`. Point the agent config at the entrypoints:

```toml
[hooks]
prolog = "/opt/spur/hooks/prolog.sh"
epilog = "/opt/spur/hooks/epilog.sh"
```

Node hooks are read by `spurd`. Restart the agent after changing them. The
paths must be absolute, and the scripts must not be group- or world-writable.

Set `SPUR_CONTAINER_DIR` to a directory on a dedicated scratch mount. Prolog
rejects a container directory that resolves to the root filesystem.

Jobs that start Docker containers must label them
`spur_job_id=${SPUR_JOB_ID}`. See [DOCKER_CONTAINERS.md](DOCKER_CONTAINERS.md).

`exporter-prolog.sh` and `exporter-epilog.sh` write and remove per-GPU JSON
under `/var/run/exporter/` for AMD Device Metrics Exporter. An exporter
failure is logged and does not fail the job. A prolog or epilog health
failure does: prolog exit 1 drains the node before launch, and epilog exit 1
drains it when job-owned residue remains.

Defaults and overrides (`SPUR_PROLOG_FAIL_ON_DIRTY`,
`SPUR_HOOK_CLEAN_ORPHANS`, free-space thresholds, command timeouts) are set
in `common-env.sh`. Leave `SPUR_PROLOG_FAIL_ON_DIRTY` at `1` on nodes that
run untrusted multi-tenant work. Do not wrap these scripts with an
unconditional `exit 0`.
