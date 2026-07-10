# RUNBOOK — append-only cluster change log

This is the operational run-log for the cluster: the operator records here, **as it happens**, every
cluster-mutating (**CHG**) or read-only (**RO**) action taken at each phase of bring-up — the exact
command, the host it ran on, the result, and how to undo it. Append a new dated section per phase as
you work; never edit or delete past entries. Once something is reusable, promote it into a script
under the numbered dirs and reference it from the entry here. For each entry, replace `YYYY-MM-DD`,
mark the heading `CHG` or `RO`, and fill the bullets with what you actually ran.

## YYYY-MM-DD — network prereqs (CHG/RO)

- `<command>` — host `<node>` — result `<outcome>` — undo `<reverse command>`
- `<next action…>`

## YYYY-MM-DD — k0s cluster bring-up (CHG/RO)

- `<command>` — host `<node>` — result `<outcome>` — undo `<reverse command>`
- `<next action…>`

## YYYY-MM-DD — GPU device plugin (CHG/RO)

- `<command>` — host `<node>` — result `<outcome>` — undo `<reverse command>`
- `<next action…>`

## YYYY-MM-DD — ARC runners (CHG/RO)

- `<command>` — host `<node>` — result `<outcome>` — undo `<reverse command>`
- `<next action…>`

## YYYY-MM-DD — gated serving deploy (CHG/RO)

- `<command>` — host `<node>` — result `<outcome>` — undo `<reverse command>`
- `<next action…>`

## YYYY-MM-DD — RDMA / distributed inference (CHG/RO)

- `<command>` — host `<node>` — result `<outcome>` — undo `<reverse command>`
- `<next action…>`
