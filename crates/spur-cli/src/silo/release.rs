// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which cluster-forge to deploy, and where to get it.

use anyhow::{bail, Result};

/// The cluster-forge release SPUR deploys by default. Pinned rather than resolved at run time, so
/// two installs a month apart deploy the same platform stack. Bump it deliberately.
pub const PINNED_VERSION: &str = "v2.2.2";

/// Upstream cluster-forge. A release given as a bare revision is cloned from here.
pub const REPO: &str = "https://github.com/silogen/cluster-forge.git";

#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    /// A release archive to download and unpack.
    Tarball(String),
    /// A git revision to clone: a tag, or a branch for development.
    Revision(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Release {
    pub source: Source,
    /// What ArgoCD records as the target revision, so the cluster reports the version it runs.
    pub revision: String,
}

/// Resolve what `--release` asked for. An empty value takes [`PINNED_VERSION`].
///
/// The deployer this replaces mapped the word `latest` to the `main` branch. That is dropped: it
/// deploys unreleased code under a name that reads like a release. Ask for `main` by name instead.
pub fn resolve(release: &str) -> Result<Release> {
    let release = match release.trim() {
        "" => PINNED_VERSION,
        given => given,
    };
    if !release.starts_with("http") {
        return Ok(Release {
            source: Source::Revision(release.to_string()),
            revision: release.to_string(),
        });
    }
    let Some(version) = version_in_url(release) else {
        bail!(
            "could not read a version out of the release URL {release}, and ArgoCD needs one to \
             record what it runs. Pass --release <tag> instead."
        );
    };
    Ok(Release {
        source: Source::Tarball(release.to_string()),
        revision: version,
    })
}

/// Find a `vMAJOR.MINOR.PATCH` (with an optional pre-release suffix) inside a release URL.
fn version_in_url(url: &str) -> Option<String> {
    url.match_indices('v')
        .find_map(|(start, _)| semver_at(&url[start..]).map(str::to_string))
}

/// The `vMAJOR.MINOR.PATCH[-suffix]` prefix of `s`, when it has one.
fn semver_at(s: &str) -> Option<&str> {
    let mut rest = s.strip_prefix('v')?;
    let mut len = 1;
    for field in 0..3 {
        if field > 0 {
            rest = rest.strip_prefix('.')?;
            len += 1;
        }
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return None;
        }
        rest = &rest[digits..];
        len += digits;
    }
    if let Some(suffix) = rest.strip_prefix('-') {
        let taken = suffix.len()
            - suffix
                .trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '.')
                .len();
        if taken > 0 {
            len += 1 + taken;
        }
    }
    Some(&s[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_release_takes_the_pinned_version() {
        let r = resolve("").expect("resolves");
        assert_eq!(r.source, Source::Revision(PINNED_VERSION.to_string()));
        assert_eq!(r.revision, PINNED_VERSION);
    }

    #[test]
    fn a_tag_is_cloned_and_recorded_verbatim() {
        let r = resolve("v2.2.1").expect("resolves");
        assert_eq!(r.source, Source::Revision("v2.2.1".to_string()));
        assert_eq!(r.revision, "v2.2.1");
    }

    #[test]
    fn a_branch_is_cloned_by_name() {
        // `latest` no longer means `main`, so a branch has to be asked for explicitly.
        let r = resolve("main").expect("resolves");
        assert_eq!(r.source, Source::Revision("main".to_string()));
    }

    #[test]
    fn a_release_url_is_downloaded_and_its_version_recorded() {
        let url =
            "https://github.com/silogen/cluster-forge/releases/download/v2.2.2/release.tar.gz";
        let r = resolve(url).expect("resolves");
        assert_eq!(r.source, Source::Tarball(url.to_string()));
        assert_eq!(r.revision, "v2.2.2");
    }

    #[test]
    fn a_pre_release_suffix_survives() {
        assert_eq!(
            version_in_url("https://x/v2.2.1-rc1/release.tar.gz").as_deref(),
            Some("v2.2.1-rc1")
        );
    }

    #[test]
    fn a_url_carrying_no_version_is_an_error_rather_than_a_silent_branch() {
        assert!(resolve("https://example.com/cluster-forge/nightly.tar.gz").is_err());
    }

    #[test]
    fn a_v_that_starts_no_version_does_not_match() {
        // "silogen" holds a `v`; the scan must not stop there and report a bad version.
        assert_eq!(
            version_in_url("https://github.com/silogen/cluster-forge/archive/v1.0.0.tar.gz")
                .as_deref(),
            Some("v1.0.0")
        );
        assert_eq!(
            version_in_url("https://example.com/vendor/build.tar.gz"),
            None
        );
    }
}
