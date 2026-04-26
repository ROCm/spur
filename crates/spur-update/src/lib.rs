//! Auto-update and version checking for Spur binaries.
//!
//! Provides:
//! - Background startup check against GitHub releases API
//! - CLI `spur version --check` and `spur self-update`
//! - Disk-backed cache (1h TTL) to avoid API spam
//! - SHA256-verified downloads with atomic binary replacement

pub mod cache;
pub mod check;
pub mod download;
pub mod install;

use std::path::PathBuf;

use check::{Channel, UpdateCheckResult};
use tracing::{debug, info, warn};

/// Spur binary names included in release tarballs.
pub const SPUR_BINARIES: &[&str] = &["spur", "spurctld", "spurd", "spurdbd", "spurrestd"];

/// Spawn a non-blocking background update check.
///
/// Call this from daemon main() after tracing is initialized.
/// It spawns a tokio task and returns immediately — startup is not delayed.
/// Logs info-level message if an update is available.
///
/// If `auto_update` is true in config and an update is found, it will
/// download and replace binaries (but NOT restart the daemon).
pub fn spawn_startup_check(
    repo: &'static str,
    current_version: &'static str,
    check_on_startup: bool,
    auto_update: bool,
    channel: &str,
    cache_dir: &str,
    binary_names: &'static [&'static str],
) {
    if !check_on_startup {
        return;
    }

    let channel = Channel::from_str(channel);
    let cache_dir = PathBuf::from(cache_dir);

    tokio::spawn(async move {
        // Check cache first
        if let Some(cached) = cache::read_cache(&cache_dir) {
            if cache::is_cache_fresh(&cached) {
                if cached.update_available {
                    info!(
                        current = %cached.current_version,
                        latest = %cached.latest_tag,
                        "Update available. Run `spur self-update` to install."
                    );
                }
                return;
            }
        }

        // Cache stale or missing — fetch from GitHub
        match check::check_for_update(repo, current_version, &channel).await {
            Ok(result) => {
                // Write cache
                let cache_entry = cache::UpdateCache {
                    checked_at: result.checked_at,
                    current_version: result.current_version.clone(),
                    latest_tag: result.latest.tag.clone(),
                    update_available: result.update_available,
                    channel: match channel {
                        Channel::Stable => "stable".into(),
                        Channel::Nightly => "nightly".into(),
                    },
                };
                cache::write_cache(&cache_dir, &cache_entry);

                if result.update_available {
                    info!(
                        current = %result.current_version,
                        latest = %result.latest.tag,
                        "Update available. Run `spur self-update` to install."
                    );

                    if auto_update {
                        info!("auto_update enabled — downloading update...");
                        if let Err(e) = do_self_update(&result, binary_names).await {
                            warn!("auto-update failed: {e}");
                        }
                    }
                } else {
                    debug!("spur is up to date ({})", current_version);
                }
            }
            Err(e) => {
                debug!("update check failed (non-fatal): {e}");
            }
        }
    });
}

/// Perform a self-update: download, verify, and replace binaries.
pub async fn do_self_update(
    result: &UpdateCheckResult,
    binary_names: &[&str],
) -> anyhow::Result<()> {
    let install_dir = install::detect_install_dir()?;
    let tmp_dir = std::env::temp_dir().join("spur-update");
    std::fs::create_dir_all(&tmp_dir)?;

    let tarball = download::download_and_verify(&result.latest, &tmp_dir).await?;
    install::atomic_replace(&tarball, binary_names, &install_dir)?;

    // Clean up
    let _ = std::fs::remove_dir_all(&tmp_dir);

    info!(
        version = %result.latest.tag,
        dir = %install_dir.display(),
        "Update installed. Restart daemons to use the new version."
    );

    Ok(())
}

/// Perform a full self-update flow: check + download + replace.
/// Used by the CLI `spur self-update` command.
pub async fn self_update_cli(
    repo: &str,
    current_version: &str,
    channel: &Channel,
    binary_names: &[&str],
) -> anyhow::Result<()> {
    let result = check::check_for_update(repo, current_version, channel).await?;

    if !result.update_available {
        println!("spur {} is already up to date.", current_version);
        return Ok(());
    }

    println!(
        "New version available: {} → {}",
        result.current_version, result.latest.tag
    );
    print!("Downloading... ");

    let install_dir = install::detect_install_dir()?;
    let tmp_dir = std::env::temp_dir().join("spur-self-update");
    std::fs::create_dir_all(&tmp_dir)?;

    let tarball = download::download_and_verify(&result.latest, &tmp_dir).await?;
    println!("done.");

    print!("Installing to {}... ", install_dir.display());
    install::atomic_replace(&tarball, binary_names, &install_dir)?;
    println!("done.");

    // Clean up
    let _ = std::fs::remove_dir_all(&tmp_dir);

    println!("Updated spur to {}", result.latest.tag);
    println!("Note: Restart running daemons (spurctld, spurd) to use the new version.");

    Ok(())
}
