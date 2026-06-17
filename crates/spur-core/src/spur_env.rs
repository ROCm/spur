// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Centralized job environment variable construction.
//!
//! Every injection site (batch jobs, hooks, salloc, srun steps, K8s pods)
//! builds its process environment through [`SpurEnv`]. This ensures all
//! `SPUR_*` variables get a corresponding `SLURM_*` twin automatically.

use std::collections::HashMap;

/// Accumulator for job environment variables. Handles the `SPUR_` / `SLURM_`
/// prefix policy so callers don't have to duplicate twin insertions.
pub struct SpurEnv {
    vars: HashMap<String, String>,
}

impl SpurEnv {
    pub fn new() -> Self {
        Self {
            vars: HashMap::new(),
        }
    }

    /// Insert `name` and, if it starts with `SPUR`, a twin with the `SPUR`
    /// prefix replaced by `SLURM` (e.g. `SPUR_JOB_ID` → `SLURM_JOB_ID`)
    /// If `name` doesn't start with `SPUR`, only the original is inserted.
    pub fn set_with_slurm_twin(&mut self, name: &str, value: impl ToString) {
        let v = value.to_string();
        if let Some(rest) = name.strip_prefix("SPUR") {
            self.vars.insert(name.to_string(), v.clone());
            self.vars.insert(format!("SLURM{rest}"), v);
        } else {
            self.vars.insert(name.to_string(), v);
        }
    }

    /// Insert a variable as-is.
    pub fn set(&mut self, name: &str, value: impl ToString) {
        self.vars.insert(name.to_string(), value.to_string());
    }

    /// Merge a batch of raw key-value pairs (e.g. user-submitted environment,
    /// device injection plan, forwarded request environment).
    pub fn extend(&mut self, vars: &HashMap<String, String>) {
        self.vars
            .extend(vars.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    /// Consume into the final `HashMap` for process spawning.
    pub fn into_map(self) -> HashMap<String, String> {
        self.vars
    }

    /// Generate bash `export` lines for per-task variables (`PROCID`, `LOCALID`).
    ///
    /// These are interpolated inside the multi-task wrapper loop where
    /// `$LOCAL_RANK` and `$SPUR_TASK_OFFSET` are shell variables, not Rust values.
    pub fn per_task_bash_exports() -> &'static str {
        concat!(
            "  export SPUR_LOCALID=$LOCAL_RANK\n",
            "  export SLURM_LOCALID=$LOCAL_RANK\n",
            "  export SPUR_PROCID=$((SPUR_TASK_OFFSET + LOCAL_RANK))\n",
            "  export SLURM_PROCID=$((SPUR_TASK_OFFSET + LOCAL_RANK))\n",
        )
    }
}

impl Default for SpurEnv {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twin_inserts_spur_and_slurm() {
        let mut env = SpurEnv::new();
        env.set_with_slurm_twin("SPUR_JOB_ID", 42);
        let map = env.into_map();
        assert_eq!(map.get("SPUR_JOB_ID").unwrap(), "42");
        assert_eq!(map.get("SLURM_JOB_ID").unwrap(), "42");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn twin_handles_spurd_prefix() {
        let mut env = SpurEnv::new();
        env.set_with_slurm_twin("SPURD_NODENAME", "node01");
        let map = env.into_map();
        assert_eq!(map.get("SPURD_NODENAME").unwrap(), "node01");
        assert_eq!(map.get("SLURMD_NODENAME").unwrap(), "node01");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn twin_without_spur_prefix_inserts_once() {
        let mut env = SpurEnv::new();
        env.set_with_slurm_twin("PATH", "/usr/bin");
        let map = env.into_map();
        assert_eq!(map.get("PATH").unwrap(), "/usr/bin");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn set_inserts_raw_key() {
        let mut env = SpurEnv::new();
        env.set("MASTER_ADDR", "10.0.0.1");
        let map = env.into_map();
        assert_eq!(map.get("MASTER_ADDR").unwrap(), "10.0.0.1");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn extend_merges_raw_vars() {
        let mut env = SpurEnv::new();
        env.set_with_slurm_twin("SPUR_JOB_ID", 1);

        let mut extra = HashMap::new();
        extra.insert("PMI_SIZE".into(), "4".into());
        extra.insert("PMI_RANK".into(), "0".into());
        env.extend(&extra);

        let map = env.into_map();
        assert_eq!(map.get("PMI_SIZE").unwrap(), "4");
        assert_eq!(map.get("PMI_RANK").unwrap(), "0");
        assert_eq!(map.get("SPUR_JOB_ID").unwrap(), "1");
    }

    #[test]
    fn later_insert_overwrites_earlier() {
        let mut env = SpurEnv::new();
        env.set_with_slurm_twin("SPUR_JOB_ID", 1);
        env.set_with_slurm_twin("SPUR_JOB_ID", 2);
        let map = env.into_map();
        assert_eq!(map.get("SPUR_JOB_ID").unwrap(), "2");
        assert_eq!(map.get("SLURM_JOB_ID").unwrap(), "2");
    }

    #[test]
    fn extend_does_not_clobber_later_twin() {
        let mut env = SpurEnv::new();
        let mut user = HashMap::new();
        user.insert("SPUR_JOB_ID".into(), "user-value".into());
        env.extend(&user);
        env.set_with_slurm_twin("SPUR_JOB_ID", 99);
        let map = env.into_map();
        assert_eq!(map["SPUR_JOB_ID"], "99");
        assert_eq!(map["SLURM_JOB_ID"], "99");
    }

    #[test]
    fn per_task_bash_exports_has_twins() {
        let exports = SpurEnv::per_task_bash_exports();
        assert!(exports.contains("SPUR_LOCALID"));
        assert!(exports.contains("SLURM_LOCALID"));
        assert!(exports.contains("SPUR_PROCID"));
        assert!(exports.contains("SLURM_PROCID"));
    }
}
