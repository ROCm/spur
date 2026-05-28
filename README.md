# Spur

[![CI](https://github.com/ROCm/spur/actions/workflows/ci.yml/badge.svg)](https://github.com/ROCm/spur/actions/workflows/ci.yml)
[![E2E](https://github.com/ROCm/spur/actions/workflows/e2e.yml/badge.svg)](https://github.com/ROCm/spur/actions/workflows/e2e.yml)
[![Rust](https://img.shields.io/badge/dynamic/toml?url=https://raw.githubusercontent.com/ROCm/spur/main/rust-toolchain.toml&query=toolchain.channel&label=rust&color=orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

An AI-native job scheduler written in Rust. Drop-in compatible with Slurm's CLI, REST API, and C FFI while providing WireGuard mesh networking, GPU-first scheduling, and modern state management.

## Highlights

- 🔌 **Slurm compatible** — your existing scripts, tools, and muscle memory work unchanged
- 🔲 **GPU-first scheduling** — first-class GPU support for job scheduling
- 💾 **Raft-based state** — all state survives restarts; no external database required
- ⚙️ **Written in Rust** — single static binary per component, fast builds, safe concurrency
- 🔒 **WireGuard mesh networking** — tunnelled cluster communication out of the box

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/ROCm/spur/main/install.sh | bash
export PATH="$HOME/.local/bin:$PATH"
```

Try it locally. Spin up a single-node cluster to see Spur in action:

```bash
spurctld -D --state-dir /tmp/spur-state          # start controller
spurd -D --controller http://localhost:6817      # start agent in a new terminal
spur run -- echo "hello from $(hostname)"        # run a command
spur queue                                       # check the queue
```

For production and multi-node deployments, see the [deployment guide](docs/deployment/native-host.rst).

## Documentation

The [full documentation](docs/index.rst) covers quickstart, deployment, and development. Whether you're evaluating Spur or running it in production, that's the best place to start.

## Getting Help

If you are stuck on anything or have found a bug, please feel free to [open an issue](https://github.com/ROCm/spur/issues) and we'll do our best to help.

## Building & Contributing

See [developer/building](docs/developer/building.rst) for build instructions and project layout, and [developer/contributing](docs/developer/contributing.rst) for contribution guidelines. PRs are welcome.

## License

Licensed under the Apache License 2.0. See [LICENSE](LICENSE) for details.
