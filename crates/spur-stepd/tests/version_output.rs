// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

fn spurstepd_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_spurstepd"))
}

/// `-V`/`--version` must print the shared `spur <pkg> (<sha>[-dirty])` string
/// on stdout and exit 0 *without* requiring the otherwise-mandatory
/// `<state-dir> <job-id> <attempt> <launch-spec>` positional args — the check
/// has to happen before the double-fork stderr log setup and arg validation.
#[test]
fn version_flags_print_before_positional_args_are_checked() {
    for flag in ["-V", "--version"] {
        let out = Command::new(spurstepd_bin())
            .arg(flag)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn spurstepd {flag}: {e}"));

        assert!(
            out.status.success(),
            "spurstepd {flag} exited with {:?}, stderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr),
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.starts_with(&format!("spur {}", env!("CARGO_PKG_VERSION"))),
            "spurstepd {flag} stdout did not start with the expected version: {stdout}",
        );
        assert!(
            out.stderr.is_empty(),
            "spurstepd {flag} wrote to stderr: {}",
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

/// Missing positional args without a version flag must still fail like before.
#[test]
fn missing_positional_args_without_version_flag_fails() {
    let out = Command::new(spurstepd_bin())
        .output()
        .expect("failed to spawn spurstepd");

    assert!(!out.status.success(), "missing positional args should fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("usage: spurstepd"),
        "expected usage message on stderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
}
