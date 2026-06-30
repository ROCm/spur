// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::process::{Command, Stdio};

fn spur_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_spur"))
}

#[test]
#[cfg(unix)]
fn broken_pipe_exits_cleanly() {
    // Pipe stderr but drop the read end immediately before the child writes anything.
    // The first write to a pipe with no reader triggers SIGPIPE/EPIPE deterministically,
    // regardless of how much output spur produces or how fast it runs.
    // Exit 101 is Rust's panic sentinel; 0 or signal termination is correct.
    let mut child = Command::new(spur_bin())
        .arg("help")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn spur");

    // Drop the read end immediately — any write by the child now gets SIGPIPE/EPIPE.
    drop(child.stderr.take());

    let status = child.wait().expect("failed to wait for spur");
    let code = status.code().unwrap_or(0); // signal termination → None → treat as 0
    assert_ne!(
        code, 101,
        "spur panicked on broken pipe (exit 101); SIGPIPE restoration is not working"
    );
}
