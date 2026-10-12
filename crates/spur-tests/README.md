# spur-tests

In-process unit and component tests for Spur, numbered after Slurm's testsuite where applicable. No running daemons are needed; run them with `cargo test -p spur-tests`.

End-to-end tests against real clusters live in Python under `tests/native_host/e2e/` and `tests/k8s/e2e/`. See the [Building guide](../../docs/developer/building.rst), specifically the "End-to-End Tests" sections.
