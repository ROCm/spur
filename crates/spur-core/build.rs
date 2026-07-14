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

fn main() {
    // `--dirty` reflects the whole working tree, not just this crate's
    // files, so no `rerun-if-changed` path can scope this correctly.
    // Watching a path that never exists forces Cargo to treat the build
    // script as always out of date, so it reruns on every build.
    println!("cargo:rerun-if-changed=__spur_always_rerun__");

    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // Docker builds don't ship `.git` (see `.dockerignore`), so CI passes the
    // descriptor in directly; only shell out to git for local dev builds.
    let describe =
        std::env::var("SPUR_GIT_DESCRIBE").unwrap_or_else(|_| git_describe(&manifest_dir));

    println!("cargo:rustc-env=SPUR_GIT_DESCRIBE={describe}");
}
