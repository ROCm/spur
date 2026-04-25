use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Atomically replace binaries from a downloaded tarball.
///
/// Strategy:
/// 1. Extract tarball to a temp directory
/// 2. For each binary: rename current → .spur-old, rename new → current
/// 3. Delete all .spur-old files on success
/// 4. On failure: roll back by renaming .spur-old back
pub fn atomic_replace(
    tarball_path: &Path,
    binary_names: &[&str],
    install_dir: &Path,
) -> Result<()> {
    // Extract tarball
    let extract_dir = tarball_path.parent().unwrap_or(Path::new("/tmp"));
    let extract_subdir = extract_dir.join("spur-extract");
    let _ = fs::remove_dir_all(&extract_subdir);
    fs::create_dir_all(&extract_subdir)?;

    let tarball_file = fs::File::open(tarball_path)?;
    let decoder = GzDecoder::new(tarball_file);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(&extract_subdir)
        .context("failed to extract tarball")?;

    // Find the bin directory inside the extracted tarball
    // Tarball structure: spur-{version}-linux-amd64/bin/{binaries}
    let bin_dir = find_bin_dir(&extract_subdir)?;

    // Phase 1: Backup current binaries
    let mut backed_up: Vec<String> = Vec::new();
    for name in binary_names {
        let current = install_dir.join(name);
        let backup = install_dir.join(format!("{}.spur-old", name));

        if current.exists() {
            fs::rename(&current, &backup).with_context(|| {
                format!(
                    "failed to backup {} → {}",
                    current.display(),
                    backup.display()
                )
            })?;
            backed_up.push(name.to_string());
            debug!(binary = name, "backed up");
        }
    }

    // Phase 2: Install new binaries
    let mut installed: Vec<String> = Vec::new();
    for name in binary_names {
        let new_bin = bin_dir.join(name);
        let target = install_dir.join(name);

        if !new_bin.exists() {
            debug!(binary = name, "not in tarball — skipping");
            continue;
        }

        match fs::copy(&new_bin, &target) {
            Ok(_) => {
                // Set executable permission
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o755));
                }
                installed.push(name.to_string());
                debug!(binary = name, "installed");
            }
            Err(e) => {
                // Rollback: restore all backed-up binaries
                warn!(binary = name, error = %e, "install failed — rolling back");
                for backed in &backed_up {
                    let backup = install_dir.join(format!("{}.spur-old", backed));
                    let current = install_dir.join(backed);
                    let _ = fs::rename(&backup, &current);
                }
                bail!("failed to install {}: {}", name, e);
            }
        }
    }

    // Phase 3: Clean up backups
    for name in &backed_up {
        let backup = install_dir.join(format!("{}.spur-old", name));
        let _ = fs::remove_file(&backup);
    }

    // Clean up extracted files
    let _ = fs::remove_dir_all(&extract_subdir);

    info!(
        count = installed.len(),
        dir = %install_dir.display(),
        "binaries updated"
    );

    Ok(())
}

/// Detect the install directory by finding where the current binary lives.
pub fn detect_install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to detect current executable path")?;
    let dir = exe
        .parent()
        .unwrap_or(Path::new("/usr/local/bin"))
        .to_path_buf();
    Ok(dir)
}

/// Find the bin/ directory inside an extracted tarball.
/// Tarballs have structure: spur-{version}/bin/
fn find_bin_dir(extract_dir: &Path) -> Result<PathBuf> {
    // Look for a bin/ directory one level deep
    if let Ok(entries) = fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let bin = path.join("bin");
                if bin.is_dir() {
                    return Ok(bin);
                }
            }
        }
    }

    // Maybe the binaries are directly in the extract dir
    let direct_bin = extract_dir.join("bin");
    if direct_bin.is_dir() {
        return Ok(direct_bin);
    }

    bail!(
        "no bin/ directory found in extracted tarball at {}",
        extract_dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_install_dir_returns_parent() {
        let dir = detect_install_dir().unwrap();
        assert!(dir.is_absolute());
    }
}
