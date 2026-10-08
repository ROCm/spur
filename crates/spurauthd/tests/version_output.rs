// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

fn spurauthd_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_spurauthd"))
}

/// `-V`/`--version` must print the shared `spur <pkg> (<sha>[-dirty])` string
/// on stdout and exit 0 *without* requiring the otherwise-mandatory `--cluster`
/// flag — the check has to happen before clap parses `Args`.
#[test]
fn version_flags_print_before_required_args_are_checked() {
    for flag in ["-V", "--version"] {
        let out = Command::new(spurauthd_bin())
            .arg(flag)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn spurauthd {flag}: {e}"));

        assert!(
            out.status.success(),
            "spurauthd {flag} exited with {:?}, stderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr),
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.starts_with(&format!("spur {}", env!("CARGO_PKG_VERSION"))),
            "spurauthd {flag} stdout did not start with the expected version: {stdout}",
        );
        assert!(
            out.stderr.is_empty(),
            "spurauthd {flag} wrote to stderr: {}",
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

/// Missing `--cluster` without a version flag must still fail like before.
#[test]
fn missing_required_arg_without_version_flag_fails() {
    let out = Command::new(spurauthd_bin())
        .output()
        .expect("failed to spawn spurauthd");

    assert!(!out.status.success(), "missing --cluster should fail");
}
