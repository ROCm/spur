// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Git descriptor captured at build time (`git describe --always --dirty`).
/// Empty when git was unavailable at build time (e.g. building from a
/// released source tarball with no `.git` directory).
pub const GIT_DESCRIBE: &str = env!("SPUR_GIT_DESCRIBE");

fn format_version(pkg_version: &str, git_describe: &str) -> String {
    let tag = format!("v{pkg_version}");
    if git_describe.is_empty() || git_describe == tag {
        format!("spur {pkg_version}")
    } else {
        format!("spur {pkg_version} ({git_describe})")
    }
}

/// User-facing version string, matching Slurm's `<product> <version>` format
/// for release builds and appending git metadata for dev builds.
pub fn version_string() -> String {
    format_version(env!("CARGO_PKG_VERSION"), GIT_DESCRIBE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_git_describe_yields_plain_version() {
        assert_eq!(format_version("0.4.1", ""), "spur 0.4.1");
    }

    #[test]
    fn exact_tag_match_yields_plain_version() {
        assert_eq!(format_version("0.4.1", "v0.4.1"), "spur 0.4.1");
    }

    #[test]
    fn commits_past_tag_includes_git_describe() {
        assert_eq!(
            format_version("0.4.1", "v0.4.1-45-gabc1234"),
            "spur 0.4.1 (v0.4.1-45-gabc1234)"
        );
    }

    #[test]
    fn dirty_tree_includes_dirty_suffix() {
        assert_eq!(
            format_version("0.4.1", "v0.4.1-dirty"),
            "spur 0.4.1 (v0.4.1-dirty)"
        );
    }

    #[test]
    fn commits_past_tag_and_dirty_includes_full_descriptor() {
        assert_eq!(
            format_version("0.4.1", "v0.4.1-45-gabc1234-dirty"),
            "spur 0.4.1 (v0.4.1-45-gabc1234-dirty)"
        );
    }

    #[test]
    fn descriptor_from_older_tag_is_not_treated_as_exact_match() {
        assert_eq!(
            format_version("0.4.1", "v0.3.0-251-g4b85bd7"),
            "spur 0.4.1 (v0.3.0-251-g4b85bd7)"
        );
    }
}
