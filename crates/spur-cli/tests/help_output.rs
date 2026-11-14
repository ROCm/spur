// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

fn spur_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_spur"))
}

const SUBCOMMANDS: &[&str] = &[
    "salloc",
    "sbatch",
    "srun",
    "squeue",
    "scancel",
    "sinfo",
    "sacct",
    "sacctmgr",
    "scontrol",
    "sprio",
    "sshare",
    "sstat",
    "sdiag",
    "sreport",
    "strigger",
    "sattach",
    "scrontab",
    "smd",
    "net",
    "node",
    "k8s",
    "image",
    "exec",
    "token",
    "self-update",
    "update",
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

#[test]
fn self_update_help_and_bad_arguments_do_not_replace_binaries() {
    let dir = tempfile::TempDir::new().expect("temporary install directory");
    let binary = dir.path().join("spur");
    std::fs::copy(spur_bin(), &binary).expect("copy CLI into the test install directory");
    let original = std::fs::read(&binary).expect("read CLI");
    for name in ["spurctld", "spurd", "spurstepd", "spurauthd"] {
        std::fs::write(dir.path().join(name), name).expect("write sibling binary fixture");
    }
    for command in ["self-update", "update"] {
        for args in [vec!["--help"], vec!["-h"], vec!["--nightly", "--help"]] {
            let out = Command::new(&binary)
                .arg(command)
                .args(args)
                .env("SPUR_CONF", dir.path().join("missing.conf"))
                .output()
                .expect("run self-update help");
            assert_eq!(out.status.code(), Some(0));
            assert!(out.stderr.is_empty());
            let stdout = String::from_utf8(out.stdout).expect("UTF-8 help");
            assert!(stdout.contains("Usage:") && stdout.contains("--nightly"));
            assert!(stdout.contains("spur control update"));
        }
        for args in [
            vec!["--unknown"],
            vec!["node", "n1", "state=resume"],
            vec!["node", "n1", "--nightly"],
        ] {
            let out = Command::new(&binary)
                .arg(command)
                .args(args)
                .env("SPUR_CONF", dir.path().join("missing.conf"))
                .output()
                .expect("run self-update with invalid arguments");
            assert_eq!(out.status.code(), Some(2));
            assert!(out.stdout.is_empty());
            assert!(String::from_utf8_lossy(&out.stderr).contains("unexpected argument"));
        }
    }
    assert_eq!(
        std::fs::read(binary).expect("read CLI after commands"),
        original
    );
    for name in ["spurctld", "spurd", "spurstepd", "spurauthd"] {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(name)).expect("read sibling binary"),
            name
        );
    }
}
