// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};
use std::process::Command;

/// Walk up from `start` looking for a `.git` entry (directory for a normal
/// clone, file for a worktree checkout).
fn find_git_path(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        let candidate = dir.join(".git");
        if candidate.exists() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

/// Paths whose mtime should invalidate the cached git descriptor: the
/// current HEAD and the refs that HEAD (or tags) can move to. Worktrees
/// store HEAD per-worktree but keep refs in the shared "commondir".
fn watch_paths(git_path: &Path) -> Vec<PathBuf> {
    if git_path.is_dir() {
        return vec![
            git_path.join("HEAD"),
            git_path.join("refs"),
            git_path.join("packed-refs"),
        ];
    }

    let Ok(contents) = std::fs::read_to_string(git_path) else {
        return vec![];
    };
    let Some(worktree_gitdir) = contents.trim().strip_prefix("gitdir: ") else {
        return vec![];
    };
    let worktree_gitdir = PathBuf::from(worktree_gitdir);

    let mut paths = vec![worktree_gitdir.join("HEAD")];
    if let Ok(commondir) = std::fs::read_to_string(worktree_gitdir.join("commondir")) {
        let common_dir = worktree_gitdir.join(commondir.trim());
        paths.push(common_dir.join("HEAD"));
        paths.push(common_dir.join("refs"));
        paths.push(common_dir.join("packed-refs"));
    }
    paths
}

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
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(git_path) = find_git_path(&manifest_dir) {
        for path in watch_paths(&git_path) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    println!(
        "cargo:rustc-env=SPUR_GIT_DESCRIBE={}",
        git_describe(&manifest_dir)
    );
}
