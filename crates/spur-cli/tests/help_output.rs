// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

fn spur_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_spur"))
}

const SUBCOMMANDS: &[&str] = &[
    "salloc", "sbatch", "srun", "squeue", "scancel", "sinfo", "sacct", "sacctmgr", "scontrol",
    "sprio", "sshare", "sstat", "sdiag", "sreport", "strigger", "sattach", "scrontab", "smd",
    "net", "node", "k8s", "image", "exec", "token",
];

/// `--help` must render through clap: help text on stdout, exit 0, nothing on
/// stderr. Parsing with `?` instead routes clap's help error through `anyhow`,
/// which prints `Error: <help>` to stderr and exits 1.
#[test]
fn subcommand_help_goes_to_stdout_with_exit_zero() {
    for &cmd in SUBCOMMANDS {
        let out = Command::new(spur_bin())
            .args([cmd, "--help"])
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn spur {cmd} --help: {e}"));

        assert!(
            out.status.success(),
            "spur {cmd} --help exited with {:?}, stderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr),
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("Usage:"),
            "spur {cmd} --help stdout missing usage text: {stdout}",
        );
        assert!(
            out.stderr.is_empty(),
            "spur {cmd} --help wrote to stderr: {}",
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

/// `sacctmgr add/modify/show --help` must name the valid `key=value` fields,
/// otherwise the only way to discover them is to submit a bad one and read the
/// error. Regression test for the help text being byte-identical for every
/// entity and listing no fields.
#[test]
fn sacctmgr_help_lists_key_value_fields() {
    let write_cases = [
        (&["sacctmgr", "add", "account", "--help"][..], "fairshare"),
        (&["sacctmgr", "modify", "qos", "--help"][..], "preemptmode"),
        (&["sacctmgr", "add", "user", "--help"][..], "defaultqos"),
    ];
    for (args, field) in write_cases {
        let out = Command::new(spur_bin())
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn spur {args:?}: {e}"));
        assert!(out.status.success(), "spur {args:?} should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("Fields (key=value)"),
            "spur {args:?} --help missing field reference:\n{stdout}",
        );
        assert!(
            stdout.contains(field),
            "spur {args:?} --help should name '{field}':\n{stdout}",
        );
    }

    let out = Command::new(spur_bin())
        .args(["sacctmgr", "show", "qos", "--help"])
        .output()
        .expect("failed to spawn spur sacctmgr show qos --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Filters (key=value)") && stdout.contains("association"),
        "sacctmgr show --help should list per-entity filters:\n{stdout}",
    );
}

/// A genuine parse error must still fail: message on stderr, non-zero exit.
#[test]
fn subcommand_parse_error_fails_on_stderr() {
    let out = Command::new(spur_bin())
        .args(["squeue", "--definitely-not-a-flag"])
        .output()
        .expect("failed to spawn spur");

    assert!(!out.status.success(), "unknown flag should fail");
    assert!(out.stdout.is_empty(), "parse error should not write stdout");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("error:"),
        "parse error should report on stderr",
    );
}
