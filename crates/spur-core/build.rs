// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::process::Command;

fn git_describe(manifest_dir: &Path) -> String {
    Command::new("git")
        .args(["describe", "--always", "--dirty"])
        .current_dir(manifest_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

// No `rerun-if-changed` is emitted: Cargo's default with none is to rerun
// every build, which we want since `--dirty` reflects the whole tree, not
// just this crate's files. Not unit tested; verified manually and in CI.
fn main() {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // Docker builds don't ship `.git` (see `.dockerignore`), so CI passes the
    // descriptor in directly; only shell out to git for local dev builds.
    let describe =
        std::env::var("SPUR_GIT_DESCRIBE").unwrap_or_else(|_| git_describe(&manifest_dir));

    println!("cargo:rustc-env=SPUR_GIT_DESCRIBE={describe}");
}
