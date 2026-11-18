// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Where a batch script runs: once on the batch host, or once on every node.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

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
