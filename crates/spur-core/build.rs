// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::process::Command;

fn git(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
}

fn git_sha(manifest_dir: &Path) -> String {
    git(manifest_dir, &["rev-parse", "--short=8", "HEAD"]).unwrap_or_default()
}

fn git_dirty(manifest_dir: &Path) -> bool {
    // Any porcelain output means uncommitted changes; no git means clean.
    git(manifest_dir, &["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

fn main() {
    // Git state spans the whole tree, so no path scopes it; watching a
    // nonexistent path forces a rerun on every build.
    println!("cargo:rerun-if-changed=__spur_always_rerun__");

    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // Docker builds omit `.git`, so CI passes these in; local builds read git.
    let sha = std::env::var("SPUR_GIT_SHA").unwrap_or_else(|_| git_sha(&manifest_dir));
    let dirty = std::env::var("SPUR_GIT_DIRTY")
        .map(|v| v == "true" || v == "1")
        .unwrap_or_else(|_| git_dirty(&manifest_dir));

    println!("cargo:rustc-env=SPUR_GIT_SHA={sha}");
    println!("cargo:rustc-env=SPUR_GIT_DIRTY={dirty}");
}
