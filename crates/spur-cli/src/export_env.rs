// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared `--export` resolution for `sbatch` and `srun`.
//!
//! Both commands identify which of the submitter's environment variables reach
//! the launched application, using Slurm's `--export` grammar. The logic lives
//! here so the two entry points stay identical.

use std::collections::HashMap;

/// Whether `spec` is the default `ALL` mode (case-insensitive, as in Slurm).
pub(crate) fn is_export_all(spec: &str) -> bool {
    spec.trim().eq_ignore_ascii_case("ALL")
}

/// Resolve `--export` per Slurm semantics against a submission environment.
///
/// `ALL` seeds the full environment. `NONE` and a leading bare list token seed
/// only the caller's `SLURM_*`/`SPUR_*` variables, which Slurm always
/// propagates so a step keeps its allocation context. Remaining tokens are
/// applied on top: `VAR` copies the current value from `source`, `VAR=value`
/// sets an explicit value (overriding an inherited one). The value may itself
/// contain `=`. `ALL` and `NONE` match case-insensitively.
pub(crate) fn resolve_export_env(
    spec: &str,
    source: HashMap<String, String>,
) -> HashMap<String, String> {
    if is_export_all(spec) {
        return source;
    }
    let tokens: Vec<&str> = spec
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let (mut env, rest) = match tokens.first() {
        Some(t) if t.eq_ignore_ascii_case("ALL") => (source.clone(), &tokens[1..]),
        Some(t) if t.eq_ignore_ascii_case("NONE") => (scheduler_vars(&source), &tokens[1..]),
        _ => (scheduler_vars(&source), &tokens[..]),
    };
    for tok in rest {
        match tok.split_once('=') {
            Some((k, v)) => {
                env.insert(k.to_string(), v.to_string());
            }
            None => {
                if let Some(v) = source.get(*tok) {
                    env.insert(tok.to_string(), v.clone());
                }
            }
        }
    }
    env
}

/// Copy the `SLURM_*`/`SPUR_*` variables Slurm always propagates.
///
/// The auth token is scheduler-prefixed but is a credential, not allocation
/// context; a restricted export must not carry it into the job unless named.
fn scheduler_vars(source: &HashMap<String, String>) -> HashMap<String, String> {
    source
        .iter()
        .filter(|(k, _)| k.as_str() != crate::authclient::TOKEN_ENV)
        .filter(|(k, _)| k.starts_with("SLURM_") || k.starts_with("SPUR_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_env() -> HashMap<String, String> {
        [
            ("HOME", "/home/me"),
            ("EDITOR", "vim"),
            ("PATH", "/usr/bin"),
            ("SLURM_JOB_ID", "42"),
            ("SPUR_NTASKS", "4"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    #[test]
    fn resolve_export_all_propagates_full_env() {
        let env = resolve_export_env("ALL", source_env());
        assert_eq!(env, source_env());
    }

    #[test]
    fn resolve_export_none_keeps_only_scheduler_vars() {
        let env = resolve_export_env("NONE", source_env());
        assert_eq!(env.len(), 2);
        assert_eq!(env["SLURM_JOB_ID"], "42");
        assert_eq!(env["SPUR_NTASKS"], "4");
        assert!(!env.contains_key("HOME"));
    }

    #[test]
    fn resolve_export_restricted_modes_drop_auth_token_unless_named() {
        let mut source = source_env();
        source.insert("SPUR_AUTH_TOKEN".into(), "secret".into());

        for spec in ["NONE", "HOME", "MASTER_PORT=1"] {
            let env = resolve_export_env(spec, source.clone());
            assert!(!env.contains_key("SPUR_AUTH_TOKEN"), "{spec}");
            assert_eq!(env["SPUR_NTASKS"], "4", "{spec}");
        }
        let named = resolve_export_env("NONE,SPUR_AUTH_TOKEN", source);
        assert_eq!(named["SPUR_AUTH_TOKEN"], "secret");
    }

    #[test]
    fn resolve_export_plain_list_copies_named_vars_and_scheduler_vars() {
        let env = resolve_export_env("HOME,EDITOR", source_env());
        assert_eq!(env["HOME"], "/home/me");
        assert_eq!(env["EDITOR"], "vim");
        assert_eq!(env["SLURM_JOB_ID"], "42");
        assert_eq!(env["SPUR_NTASKS"], "4");
        assert!(!env.contains_key("PATH"));
    }

    #[test]
    fn resolve_export_bare_name_missing_from_source_is_skipped() {
        let env = resolve_export_env("HOME,NOPE", source_env());
        assert!(env.contains_key("HOME"));
        assert!(!env.contains_key("NOPE"));
    }

    #[test]
    fn resolve_export_combined_all_adds_and_overrides() {
        let env = resolve_export_env("ALL,EDITOR=emacs,WORLD_SIZE=16", source_env());
        assert_eq!(env["HOME"], "/home/me");
        assert_eq!(env["PATH"], "/usr/bin");
        assert_eq!(env["EDITOR"], "emacs");
        assert_eq!(env["WORLD_SIZE"], "16");
    }

    #[test]
    fn resolve_export_inline_assignment_without_all() {
        let env = resolve_export_env("MASTER_PORT=29999,HOME", source_env());
        assert_eq!(env["MASTER_PORT"], "29999");
        assert_eq!(env["HOME"], "/home/me");
        // Scheduler vars are seeded even in list mode; plain (non-scheduler)
        // vars that were not listed are not.
        assert_eq!(env["SLURM_JOB_ID"], "42");
        assert!(!env.contains_key("EDITOR"));
    }

    #[test]
    fn resolve_export_value_may_contain_equals() {
        let env = resolve_export_env("KEY=a=b=c", source_env());
        assert_eq!(env["KEY"], "a=b=c");
    }

    #[test]
    fn resolve_export_modes_match_case_insensitively() {
        assert_eq!(resolve_export_env("all", source_env()), source_env());
        assert!(!resolve_export_env("none", source_env()).contains_key("HOME"));
        assert_eq!(
            resolve_export_env("All,EDITOR=emacs", source_env())["PATH"],
            "/usr/bin"
        );
    }

    #[test]
    fn resolve_export_trims_tokens() {
        let env = resolve_export_env("ALL, EDITOR=emacs , HOME", source_env());
        assert_eq!(env["EDITOR"], "emacs");
        assert!(!env.contains_key(" EDITOR"));
    }

    #[test]
    fn is_export_all_ignores_case_and_padding() {
        assert!(is_export_all("ALL"));
        assert!(is_export_all(" all "));
        assert!(!is_export_all("ALL,FOO=bar"));
        assert!(!is_export_all("NONE"));
    }
}
