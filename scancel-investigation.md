# scancel user-filter investigation

## Summary

`scancel -u USER` previously sent `GetJobs` with an empty state filter. The controller interprets an empty state filter as all states, so it returned every retained job for the user, including completed, failed, cancelled, timed-out, node-failed, deadline, and out-of-memory jobs. The CLI then attempted to cancel each result, producing the reported errors.

Commit `c615aa5` added a client-side terminal-state guard and is already present on `main`, so a current build does not send cancellation requests for those returned terminal jobs. The observed cluster behavior therefore matches an older deployed CLI that predates that commit.

The additional fix changes the default filter-based request to ask the controller only for cancellable states. The existing client-side guard remains as defense against unexpected controller responses.

## Tests

- Baseline focused tests: 3 passed.
- Updated focused tests: 4 passed, including verification that the default request excludes every terminal state.
- Full `spur-cli` suite: 203 passed across unit and integration tests.
- `cargo fmt --check`: passed.
- `cargo clippy -p spur-cli --all-targets --locked -- -D warnings`: passed.
- `git diff --check`: passed.

## Full output

- `/tmp/spur-scancel-before.txt`
- `/tmp/spur-scancel-after.txt`
- `/tmp/spur-cli-test.txt`
- `/tmp/spur-scancel-clippy.txt`
