# AGENTS.md -- spur-tests

In-process tests only (no daemons, no network). The test naming scheme,
harness fixtures, and the shared job ID counter rule are in
`docs/knowledge/testing/testing.md`; follow them when adding tests.

## Pitfalls

- CLI parsers are not exercised from this crate (`spur-cli` is a binary crate
  and cannot be depended on). `t17_submit` only checks `test_script()` output
  and `JobSpec`/`Partition` defaults (no `#SBATCH` parser runs), and
  `t55_format` only tests `config::format_time` and display strings (no `%X`
  format engine), despite what the `t55_format` module doc comment says.
- `t57_auth` covers JWT only; native `SPURTKN1` credentials are not tested here.
