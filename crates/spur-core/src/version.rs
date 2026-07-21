// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Short git commit hash at build time; empty when git was unavailable.
pub const GIT_SHA: &str = env!("SPUR_GIT_SHA");

/// Whether the working tree had uncommitted changes at build time.
pub const GIT_DIRTY: bool = matches!(env!("SPUR_GIT_DIRTY").as_bytes(), b"true");

fn format_version(pkg_version: &str, git_sha: &str, git_dirty: bool) -> String {
    // Cargo.toml version is authoritative; git metadata is only build provenance.
    if git_sha.is_empty() {
        return format!("spur {pkg_version}");
    }
    let dirty = if git_dirty { "-dirty" } else { "" };
    format!("spur {pkg_version} ({git_sha}{dirty})")
}

/// User-facing version string: package version plus git commit and dirty state.
pub fn version_string() -> String {
    format_version(env!("CARGO_PKG_VERSION"), GIT_SHA, GIT_DIRTY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_git_info_yields_plain_version() {
        assert_eq!(format_version("0.5.0", "", false), "spur 0.5.0");
    }

    #[test]
    fn clean_build_includes_sha() {
        assert_eq!(
            format_version("0.5.0", "fd995b2a", false),
            "spur 0.5.0 (fd995b2a)"
        );
    }

    #[test]
    fn dirty_build_appends_dirty_suffix() {
        assert_eq!(
            format_version("0.5.0", "fd995b2a", true),
            "spur 0.5.0 (fd995b2a-dirty)"
        );
    }

    #[test]
    fn dirty_flag_without_sha_stays_plain() {
        assert_eq!(format_version("0.5.0", "", true), "spur 0.5.0");
    }
}
