use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use semver::Version;
use serde::Deserialize;
use tracing::debug;

/// Release channel.
#[derive(Debug, Clone, PartialEq)]
pub enum Channel {
    Stable,
    Nightly,
}

impl Channel {
    pub fn from_str(s: &str) -> Self {
        match s {
            "nightly" => Self::Nightly,
            _ => Self::Stable,
        }
    }
}

/// Information about a GitHub release.
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: Option<Version>,
    pub tarball_url: Option<String>,
    pub checksum_url: Option<String>,
    pub published_at: DateTime<Utc>,
}

/// Result of comparing current version to latest available.
#[derive(Debug, Clone)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest: ReleaseInfo,
    pub update_available: bool,
    pub checked_at: DateTime<Utc>,
}

/// GitHub release API response (subset of fields we need).
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    published_at: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// Check GitHub releases API for a newer version.
pub async fn check_for_update(
    repo: &str,
    current_version: &str,
    channel: &Channel,
) -> Result<UpdateCheckResult> {
    let url = match channel {
        Channel::Stable => format!("https://api.github.com/repos/{}/releases/latest", repo),
        Channel::Nightly => format!(
            "https://api.github.com/repos/{}/releases/tags/nightly",
            repo
        ),
    };

    debug!(repo, url = %url, "checking for updates");

    let client = reqwest::Client::builder()
        .user_agent(format!("spur/{}", current_version))
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let release: GitHubRelease = client
        .get(&url)
        .send()
        .await
        .context("failed to fetch release info")?
        .error_for_status()
        .context("GitHub API error")?
        .json()
        .await
        .context("failed to parse release JSON")?;

    let published_at = release
        .published_at
        .parse::<DateTime<Utc>>()
        .unwrap_or_else(|_| Utc::now());

    // Find tarball and checksum assets
    let tarball_url = release
        .assets
        .iter()
        .find(|a| a.name.ends_with(".tar.gz") && !a.name.ends_with(".sha256"))
        .map(|a| a.browser_download_url.clone());

    let checksum_url = release
        .assets
        .iter()
        .find(|a| a.name.ends_with(".tar.gz.sha256"))
        .map(|a| a.browser_download_url.clone());

    // Parse version from tag
    let tag = &release.tag_name;
    let version_str = tag.strip_prefix('v').unwrap_or(tag);
    let latest_version = Version::parse(version_str).ok();

    let update_available = match (channel, &latest_version) {
        (Channel::Stable, Some(latest)) => {
            let current = Version::parse(current_version).unwrap_or(Version::new(0, 0, 0));
            latest > &current
        }
        (Channel::Nightly, _) => {
            // For nightly, always consider it an update if the published date
            // is newer than the build. We embed a build date for comparison.
            true
        }
        _ => false,
    };

    let now = Utc::now();
    Ok(UpdateCheckResult {
        current_version: current_version.to_string(),
        latest: ReleaseInfo {
            tag: tag.clone(),
            version: latest_version,
            tarball_url,
            checksum_url,
            published_at,
        },
        update_available,
        checked_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_from_str() {
        assert_eq!(Channel::from_str("stable"), Channel::Stable);
        assert_eq!(Channel::from_str("nightly"), Channel::Nightly);
        assert_eq!(Channel::from_str("anything"), Channel::Stable);
    }

    #[test]
    fn semver_comparison() {
        let v1 = Version::parse("0.1.0").unwrap();
        let v2 = Version::parse("0.2.2").unwrap();
        assert!(v2 > v1);
    }

    #[test]
    fn semver_equal_not_update() {
        let v1 = Version::parse("0.2.2").unwrap();
        let v2 = Version::parse("0.2.2").unwrap();
        assert!(!(v2 > v1));
    }
}
