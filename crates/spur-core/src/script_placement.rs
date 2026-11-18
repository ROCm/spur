// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Where a batch script runs: once on the batch host, or once on every node.

use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The placement a job asks for, or the cluster default when it asks for none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScriptPlacement {
    /// The batch host when the script visibly launches steps, otherwise every node.
    #[default]
    Auto,
    BatchHost,
    AllNodes,
}

impl ScriptPlacement {
    pub const ALL: [Self; 3] = [Self::Auto, Self::BatchHost, Self::AllNodes];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::BatchHost => "batch-host",
            Self::AllNodes => "all-nodes",
        }
    }

    /// Where a script with this placement runs.
    pub fn resolve(self, script: Option<&str>) -> ResolvedPlacement {
        match self {
            Self::Auto => ResolvedPlacement::scanned(script),
            Self::BatchHost => ResolvedPlacement::BatchHost,
            Self::AllNodes => ResolvedPlacement::AllNodes,
        }
    }
}

impl fmt::Display for ScriptPlacement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ScriptPlacement {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|placement| placement.as_str() == s)
            .ok_or_else(|| {
                format!("invalid script placement '{s}' (expected auto, batch-host, or all-nodes)")
            })
    }
}

/// Where a run's batch script runs, decided once per run at dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolvedPlacement {
    /// Only the first allocated node runs the script; the rest hold their slice
    /// of the allocation for the steps the script launches.
    BatchHost,
    /// Every allocated node runs the script.
    AllNodes,
}

impl ResolvedPlacement {
    /// The `auto` rule: the batch host when the script visibly launches steps.
    pub fn scanned(script: Option<&str>) -> Self {
        if script.is_some_and(script_launches_steps) {
            Self::BatchHost
        } else {
            Self::AllNodes
        }
    }

    /// Whether an agent that read `scan` off the script would launch a run placed
    /// `self` differently. Off the batch host a node runs the script or holds; on
    /// it, only a PMIx job's launch depends on the placement.
    pub fn launches_differently_from(
        self,
        scan: Self,
        num_nodes: usize,
        mpi: Option<&str>,
    ) -> bool {
        self != scan && (num_nodes > 1 || mpi == Some(crate::mpi::MPI_PMIX))
    }

    /// The shortest script that scans to `self`, so a copy of a job can keep where its
    /// script runs without copying the script.
    pub fn stand_in_script(self) -> &'static str {
        match self {
            Self::BatchHost => "srun",
            Self::AllNodes => "",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BatchHost => "batch-host",
            Self::AllNodes => "all-nodes",
        }
    }
}

impl fmt::Display for ResolvedPlacement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ResolvedPlacement {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "batch-host" => Ok(Self::BatchHost),
            "all-nodes" => Ok(Self::AllNodes),
            _ => Err(format!(
                "invalid resolved script placement '{s}' (expected batch-host or all-nodes)"
            )),
        }
    }
}

static STEP_LAUNCH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s;&|])(?:/.*/)?(srun|mpirun|mpiexec)\b").expect("valid step-launch regex")
});

/// Placeholder for a quoted span when sanitizing a line for step-launcher detection.
/// Must not be whitespace or a shell command separator so word adjacency is preserved.
const QUOTED_SPAN_PLACEHOLDER: char = 'x';

fn hash_starts_shell_comment(line: &str, hash_index: usize) -> bool {
    if hash_index == 0 {
        return true;
    }
    let prev = line.as_bytes()[hash_index - 1] as char;
    matches!(prev, ' ' | '\t' | ';' | '&' | '|' | '(' | ')')
}

/// Build a sanitized copy of a shell line for step-launcher regex matching: quoted
/// spans collapse to a single placeholder (preserving adjacency with neighbors),
/// and `#` starts a comment only at a word boundary.
fn sanitize_shell_line_for_step_launch(line: &str) -> String {
    let mut sanitized = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_single {
            if c == '\'' {
                in_single = false;
                sanitized.push(QUOTED_SPAN_PLACEHOLDER);
            }
            i += 1;
            continue;
        }
        if in_double {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_double = false;
                sanitized.push(QUOTED_SPAN_PLACEHOLDER);
            }
            i += 1;
            continue;
        }
        if c == '\'' {
            in_single = true;
            i += 1;
            continue;
        }
        if c == '"' {
            in_double = true;
            i += 1;
            continue;
        }
        if c == '#' && hash_starts_shell_comment(line, i) {
            break;
        }
        sanitized.push(c);
        i += 1;
    }

    sanitized
}

/// True when a batch script delegates rank fan-out to an inner step launcher.
/// Only a text scan: a launcher behind a wrapper, sub-script, or variable is missed.
fn script_launches_steps(script: &str) -> bool {
    for line in script.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let sanitized = sanitize_shell_line_for_step_launch(trimmed);
        if STEP_LAUNCH_RE.is_match(sanitized.trim()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_placement_ignores_the_script() {
        assert_eq!(
            ScriptPlacement::AllNodes.resolve(Some("srun ./train\n")),
            ResolvedPlacement::AllNodes
        );
        assert_eq!(
            ScriptPlacement::BatchHost.resolve(Some("./train\n")),
            ResolvedPlacement::BatchHost
        );
    }

    #[test]
    fn auto_follows_the_script() {
        assert_eq!(
            ScriptPlacement::Auto.resolve(Some("srun ./train\n")),
            ResolvedPlacement::BatchHost
        );
        assert_eq!(
            ScriptPlacement::Auto.resolve(Some("./train\n")),
            ResolvedPlacement::AllNodes
        );
    }

    #[test]
    fn the_default_placement_is_auto() {
        assert_eq!(ScriptPlacement::default(), ScriptPlacement::Auto);
    }

    #[test]
    fn placements_parse_from_their_names() {
        for placement in ScriptPlacement::ALL {
            assert_eq!(placement.as_str().parse(), Ok(placement));
            assert_eq!(placement.to_string(), placement.as_str());
        }
        let err = "batchhost".parse::<ScriptPlacement>().unwrap_err();
        assert!(err.contains("auto, batch-host, or all-nodes"), "{err}");
    }

    #[test]
    fn only_a_multi_node_or_pmix_launch_depends_on_the_placement() {
        use ResolvedPlacement::{AllNodes, BatchHost};

        assert!(BatchHost.launches_differently_from(AllNodes, 2, None));
        assert!(!BatchHost.launches_differently_from(AllNodes, 1, None));
        assert!(!BatchHost.launches_differently_from(AllNodes, 1, Some("mpirun")));
        assert!(BatchHost.launches_differently_from(AllNodes, 1, Some("pmix")));
        assert!(!AllNodes.launches_differently_from(AllNodes, 4, Some("pmix")));
    }

    #[test]
    fn a_stand_in_script_scans_to_its_placement() {
        for placement in [ResolvedPlacement::BatchHost, ResolvedPlacement::AllNodes] {
            assert_eq!(
                ResolvedPlacement::scanned(Some(placement.stand_in_script())),
                placement
            );
        }
    }

    #[test]
    fn a_resolved_placement_is_never_auto() {
        assert_eq!("batch-host".parse(), Ok(ResolvedPlacement::BatchHost));
        assert_eq!("all-nodes".parse(), Ok(ResolvedPlacement::AllNodes));
        assert!("auto".parse::<ResolvedPlacement>().is_err());
        assert_eq!(ResolvedPlacement::BatchHost.to_string(), "batch-host");
    }

    #[test]
    fn placements_serialize_by_name() {
        assert_eq!(
            serde_json::to_string(&ScriptPlacement::BatchHost).unwrap(),
            "\"batch-host\""
        );
        assert_eq!(
            serde_json::to_string(&ResolvedPlacement::AllNodes).unwrap(),
            "\"all-nodes\""
        );
    }

    #[test]
    fn a_script_that_launches_steps_runs_on_the_batch_host() {
        assert_eq!(
            ResolvedPlacement::scanned(Some("#!/bin/bash\nsrun ./train\n")),
            ResolvedPlacement::BatchHost
        );
    }

    #[test]
    fn a_script_without_a_launcher_runs_on_every_node() {
        assert_eq!(
            ResolvedPlacement::scanned(Some("#!/bin/bash\n./train\n")),
            ResolvedPlacement::AllNodes
        );
    }

    #[test]
    fn a_job_without_a_script_runs_on_every_node() {
        assert_eq!(
            ResolvedPlacement::scanned(None),
            ResolvedPlacement::AllNodes
        );
    }

    #[test]
    fn script_launches_steps_detects_launchers() {
        assert!(script_launches_steps("srun --mpi=pmix /tmp/a\n"));
        assert!(script_launches_steps("mpirun -np 4 /tmp/a\n"));
        assert!(script_launches_steps("mpiexec -n 4 /tmp/a\n"));
        assert!(script_launches_steps("&& srun hostname\n"));
        assert!(!script_launches_steps("#SBATCH --mpi=pmix\n/tmp/a\n"));
        assert!(!script_launches_steps("# srun hidden in comment\n"));
    }

    #[test]
    fn script_launches_steps_ignores_launchers_in_quotes() {
        assert!(!script_launches_steps(
            "echo \"this job launches no steps, but the word srun appears in this string\"\n",
        ));
        assert!(!script_launches_steps("echo \"token=use srun here\"\n"));
        assert!(!script_launches_steps("echo \"token=prefixsrunsuffix\"\n"));
        assert!(!script_launches_steps(
            "echo 'srun is only in single quotes'\n"
        ));
        assert!(script_launches_steps(
            "echo \"quoted\"; srun -n 2 hostname\n"
        ));
        assert!(script_launches_steps(
            "srun hostname # real invocation, srun in comment ignored\n",
        ));
        assert!(!script_launches_steps("echo \"x\"srun\n"));
        assert!(!script_launches_steps("foo|\"x\"srun\n"));
        assert!(!script_launches_steps("\"$VAR\"srun\n"));
        assert!(script_launches_steps("echo foo#bar; srun -n 2 hostname\n"));
        assert!(!script_launches_steps("echo ${VAR#prefix}\n"));
        // Unclosed quotes: remainder of line is treated as quoted; not full shell parsing.
        assert!(!script_launches_steps(
            "echo \"unclosed; srun -n 2 hostname\n"
        ));
    }
}
