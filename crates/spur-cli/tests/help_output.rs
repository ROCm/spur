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
