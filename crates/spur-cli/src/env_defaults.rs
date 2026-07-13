// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Environment-variable defaults for `srun` and `salloc` flags.
//!
//! clap's `#[arg(env = ...)]` accepts a single variable name, but Slurm
//! defines several aliases per flag (e.g. `--nodes` reads `SLURM_NNODES` and
//! `SLURM_JOB_NUM_NODES`) and Spur additionally honors its native `SPUR_*`
//! twins. The `apply_*` helpers resolve a flag from an ordered list of
//! candidate variables and write it into the parsed args, but only when the
//! flag was not set on the command line so that CLI always overrides env.

use anyhow::{anyhow, Result};
use clap::parser::ValueSource;
use clap::ArgMatches;
use std::fmt::Display;
use std::str::FromStr;

/// Return the first variable in `names` that is set and non-empty, together
/// with the variable name that supplied it (used for error messages).
fn first_set<'a>(names: &[&'a str]) -> Option<(&'a str, String)> {
    names.iter().find_map(|&name| match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some((name, v)),
        _ => None,
    })
}

/// Return the value of the first variable in `names` that is set and non-empty.
///
/// Empty values are skipped so an exported-but-empty variable does not mask a
/// later populated alias.
pub fn env_first(names: &[&str]) -> Option<String> {
    first_set(names).map(|(_, v)| v)
}

/// Resolve a boolean flag from `names` using Slurm's flag semantics.
///
/// Slurm enables a no-argument option when its variable is empty, `"yes"`
/// (case-insensitive), or a non-zero number; disables it for `"0"`; and
/// ignores (leaves unset) any other value. Returns `None` when no listed
/// variable is present or every present value is unrecognized.
pub fn env_flag(names: &[&str]) -> Option<bool> {
    for name in names {
        let Ok(val) = std::env::var(name) else {
            continue;
        };
        let trimmed = val.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("yes") {
            return Some(true);
        }
        if let Ok(n) = trimmed.parse::<i64>() {
            return Some(n != 0);
        }
        // Unrecognized value: skip and let a later alias decide.
    }
    None
}

/// True when the argument identified by `id` was provided on the command line
/// (as opposed to a default or being absent). Env is applied only when this is
/// false, giving CLI > env precedence.
pub fn was_cli_set(matches: &ArgMatches, id: &str) -> bool {
    matches.value_source(id) == Some(ValueSource::CommandLine)
}

/// Apply an env default to an `Option<String>` flag when it was not set on the
/// command line.
pub fn apply_str(matches: &ArgMatches, id: &str, names: &[&str], target: &mut Option<String>) {
    if was_cli_set(matches, id) {
        return;
    }
    if let Some(v) = env_first(names) {
        *target = Some(v);
    }
}

/// Apply an env default to a plain `String` flag (one carrying a default value).
pub fn apply_string(matches: &ArgMatches, id: &str, names: &[&str], target: &mut String) {
    if was_cli_set(matches, id) {
        return;
    }
    if let Some(v) = env_first(names) {
        *target = v;
    }
}

/// Apply a comma-separated env default to a `Vec<String>` flag. The env value
/// replaces (rather than appends to) the target, matching how a repeated flag
/// with `value_delimiter` behaves.
pub fn apply_csv(matches: &ArgMatches, id: &str, names: &[&str], target: &mut Vec<String>) {
    if was_cli_set(matches, id) {
        return;
    }
    if let Some(v) = env_first(names) {
        // Trim and drop empties so a padded or trailing-comma list stays clean.
        *target = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
}

/// Apply a boolean-flag env default using Slurm's flag semantics.
pub fn apply_flag(matches: &ArgMatches, id: &str, names: &[&str], target: &mut bool) {
    if was_cli_set(matches, id) {
        return;
    }
    if let Some(b) = env_flag(names) {
        *target = b;
    }
}

/// Apply a numeric env default. The error names the specific variable that
/// supplied the bad value so a misconfigured `SLURM_NNODES=abc` is easy to
/// diagnose.
pub fn apply_num<T>(matches: &ArgMatches, id: &str, names: &[&str], target: &mut T) -> Result<()>
where
    T: FromStr,
    T::Err: Display,
{
    if was_cli_set(matches, id) {
        return Ok(());
    }
    if let Some((name, v)) = first_set(names) {
        // Trim so values with incidental whitespace (e.g. `SLURM_NTASKS="4 "`)
        // parse instead of erroring.
        let trimmed = v.trim();
        *target = trimmed
            .parse()
            .map_err(|e| anyhow!("invalid value {:?} for {}: {}", v, name, e))?;
    }
    Ok(())
}

/// Test-only guard that isolates env-var tests from the runner's environment.
///
/// Clears every `SPUR_/SLURM_/SALLOC_/SRUN_`-prefixed variable on construction
/// and on drop, so a CI runner that injects `SLURM_*` vars cannot perturb the
/// resolvers and a panicking test cannot leak state into the next one. Tests
/// using it must be `#[serial]`.
#[cfg(test)]
pub(crate) struct EnvGuard;

#[cfg(test)]
impl EnvGuard {
    pub(crate) fn new() -> Self {
        Self::clear();
        EnvGuard
    }

    pub(crate) fn set(&self, key: &str, val: &str) {
        std::env::set_var(key, val);
    }

    fn clear() {
        let stale: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| {
                k.starts_with("SPUR_")
                    || k.starts_with("SLURM_")
                    || k.starts_with("SALLOC_")
                    || k.starts_with("SRUN_")
            })
            .collect();
        for k in stale {
            std::env::remove_var(k);
        }
    }
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        Self::clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial(env_injection)]
    fn env_first_returns_first_set() {
        let guard = EnvGuard::new();
        guard.set("SPUR_TEST_B", "second");
        assert_eq!(
            env_first(&["SPUR_TEST_A", "SPUR_TEST_B"]),
            Some("second".to_string())
        );
    }

    #[test]
    #[serial(env_injection)]
    fn env_first_prefers_earlier_alias() {
        let guard = EnvGuard::new();
        guard.set("SPUR_TEST_A", "first");
        guard.set("SPUR_TEST_B", "second");
        assert_eq!(
            env_first(&["SPUR_TEST_A", "SPUR_TEST_B"]),
            Some("first".to_string())
        );
    }

    #[test]
    #[serial(env_injection)]
    fn env_first_skips_empty_value() {
        let guard = EnvGuard::new();
        guard.set("SPUR_TEST_A", "");
        guard.set("SPUR_TEST_B", "populated");
        assert_eq!(
            env_first(&["SPUR_TEST_A", "SPUR_TEST_B"]),
            Some("populated".to_string())
        );
    }

    #[test]
    #[serial(env_injection)]
    fn env_first_none_when_unset() {
        let _guard = EnvGuard::new();
        assert_eq!(env_first(&["SPUR_TEST_A"]), None);
    }

    #[test]
    #[serial(env_injection)]
    fn env_flag_true_for_empty_yes_and_nonzero() {
        let guard = EnvGuard::new();

        guard.set("SPUR_TEST_FLAG", "");
        assert_eq!(env_flag(&["SPUR_TEST_FLAG"]), Some(true));

        guard.set("SPUR_TEST_FLAG", "YeS");
        assert_eq!(env_flag(&["SPUR_TEST_FLAG"]), Some(true));

        guard.set("SPUR_TEST_FLAG", "1");
        assert_eq!(env_flag(&["SPUR_TEST_FLAG"]), Some(true));

        guard.set("SPUR_TEST_FLAG", "5");
        assert_eq!(env_flag(&["SPUR_TEST_FLAG"]), Some(true));
    }

    #[test]
    #[serial(env_injection)]
    fn env_flag_false_for_zero() {
        let guard = EnvGuard::new();
        guard.set("SPUR_TEST_FLAG", "0");
        assert_eq!(env_flag(&["SPUR_TEST_FLAG"]), Some(false));
    }

    #[test]
    #[serial(env_injection)]
    fn env_flag_ignores_unrecognized_value() {
        let guard = EnvGuard::new();
        guard.set("SPUR_TEST_FLAG", "maybe");
        assert_eq!(env_flag(&["SPUR_TEST_FLAG"]), None);
    }

    #[test]
    #[serial(env_injection)]
    fn env_flag_none_when_unset() {
        let _guard = EnvGuard::new();
        assert_eq!(env_flag(&["SPUR_TEST_FLAG"]), None);
    }
}
