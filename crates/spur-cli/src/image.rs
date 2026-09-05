// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `spur image` subcommands for container image management.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

/// Container image management.
#[derive(Parser, Debug)]
#[command(name = "image", about = "Manage container images")]
pub struct ImageArgs {
    #[command(subcommand)]
    pub command: ImageCommand,
}

#[derive(Subcommand, Debug)]
pub enum ImageCommand {
    /// Import a container image as squashfs.
    ///
    /// Supports Docker/OCI registries, local Docker daemon, and Podman.
    Import {
        /// Image URI. Formats:
        ///   ubuntu:22.04                    — Docker Hub
        ///   nvcr.io/nvidia/pytorch:24.01    — custom registry
        ///   docker://image:tag              — explicit Docker registry
        ///   dockerd://image:tag             — local Docker daemon
        ///   podman://image:tag              — local Podman
        image: String,

        /// Target architecture (default: amd64)
        #[arg(short = 'a', long, default_value = "amd64")]
        arch: String,
    },
    /// List imported images.
    List,
    /// Remove an imported image.
    Remove {
        /// Image name
        name: String,
    },
    /// Export a named container back to squashfs.
    Export {
        /// Container name
        name: String,
        /// Output file (default: <name>.sqsh)
        #[arg(short = 'o', long)]
        output: Option<String>,
    },
}

pub async fn main() -> Result<()> {
    main_with_args(std::env::args().collect()).await
}

pub async fn main_with_args(args: Vec<String>) -> Result<()> {
    let args = ImageArgs::try_parse_from(&args)?;

    match args.command {
        ImageCommand::Import { image, arch } => cmd_import(&image, &arch).await,
        ImageCommand::List => cmd_list(),
        ImageCommand::Remove { name } => cmd_remove(&name),
        ImageCommand::Export { name, output } => cmd_export(&name, output.as_deref()),
    }
}

async fn cmd_import(image: &str, arch: &str) -> Result<()> {
    // Handle local Docker daemon import
    if let Some(docker_image) = image.strip_prefix("dockerd://") {
        return cmd_import_dockerd(docker_image).await;
    }

    // Handle local Podman import
    if let Some(podman_image) = image.strip_prefix("podman://") {
        return cmd_import_podman(podman_image).await;
    }

    // Registry import (native OCI puller)
    let image_ref = spur_net::oci::parse_image_ref(image);
    eprintln!(
        "Importing {}:{} from {} (arch: {})",
        image_ref.repository, image_ref.tag, image_ref.registry, arch
    );

    let image_dir = resolve_image_dir();
    let path = spur_net::pull_image(image, &image_dir).await?;

    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "Imported: {} ({:.1} MB)",
        path.display(),
        size as f64 / 1_048_576.0
    );

    Ok(())
}

/// Import from local Docker daemon via `docker save`.
async fn cmd_import_dockerd(image: &str) -> Result<()> {
    eprintln!("Importing from Docker daemon: {}", image);

    let image_dir = resolve_image_dir();
    let name = spur_net::oci::image_file_stem(image);
    let output_path = image_dir.join(format!("{}.sqsh", name));
    if output_path.exists() {
        eprintln!("Image already imported: {}", output_path.display());
        return Ok(());
    }

    std::fs::create_dir_all(&image_dir)?;
    let tmp_dir = std::env::temp_dir().join(format!(".import_dockerd_{}", name));
    let rootfs = tmp_dir.join("rootfs");
    std::fs::create_dir_all(&rootfs)?;

    // docker save → tar, then extract layers
    let output = tokio::process::Command::new("docker")
        .args(["save", image])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run docker — is Docker installed and running?")?;

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("docker save failed: {}", stderr.trim());
    }

    let rootfs_str = rootfs.to_string_lossy();
    let output_path_str = output_path.to_string_lossy();

    let import_result = extract_docker_save_tar(&output.stdout, &rootfs_str)
        .and_then(|()| pack_squashfs(&rootfs_str, &output_path_str));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    import_result?;

    let size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!(
        "Imported: {} ({:.1} MB)",
        output_path.display(),
        size as f64 / 1_048_576.0
    );
    Ok(())
}

/// Import from local Podman via `podman save`.
async fn cmd_import_podman(image: &str) -> Result<()> {
    eprintln!("Importing from Podman: {}", image);

    let image_dir = resolve_image_dir();
    let name = spur_net::oci::image_file_stem(image);
    let output_path = image_dir.join(format!("{}.sqsh", name));
    if output_path.exists() {
        eprintln!("Image already imported: {}", output_path.display());
        return Ok(());
    }

    std::fs::create_dir_all(&image_dir)?;
    let tmp_dir = std::env::temp_dir().join(format!(".import_podman_{}", name));
    let rootfs = tmp_dir.join("rootfs");
    std::fs::create_dir_all(&rootfs)?;

    let output = tokio::process::Command::new("podman")
        .args(["save", image])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run podman — is Podman installed?")?;

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("podman save failed: {}", stderr.trim());
    }

    let rootfs_str = rootfs.to_string_lossy();
    let output_path_str = output_path.to_string_lossy();

    let import_result = extract_docker_save_tar(&output.stdout, &rootfs_str)
        .and_then(|()| pack_squashfs(&rootfs_str, &output_path_str));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    import_result?;

    let size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!(
        "Imported: {} ({:.1} MB)",
        output_path.display(),
        size as f64 / 1_048_576.0
    );
    Ok(())
}

/// Extract a `docker save` tar archive into a rootfs.
/// The tar contains a manifest.json listing layer tarballs.
fn extract_docker_save_tar(tar_data: &[u8], rootfs: &str) -> Result<()> {
    let dest = Path::new(rootfs);
    let tmp = dest.join(".docker_save");
    std::fs::create_dir_all(&tmp)?;

    let result = (|| {
        let mut archive = tar::Archive::new(tar_data);
        archive.set_overwrite(true);
        archive
            .unpack(&tmp)
            .context("failed to extract docker save archive")?;

        let archive_root = tmp
            .canonicalize()
            .context("failed to resolve docker save directory")?;
        let manifest_path = resolve_docker_save_file(&archive_root, "manifest.json")
            .context("no manifest.json in docker save")?;
        let manifest_str =
            std::fs::read_to_string(manifest_path).context("no manifest.json in docker save")?;
        let manifest: Vec<serde_json::Value> =
            serde_json::from_str(&manifest_str).context("invalid manifest.json")?;

        let layers = manifest
            .first()
            .and_then(|manifest| manifest.get("Layers"))
            .and_then(|layers| layers.as_array())
            .ok_or_else(|| anyhow::anyhow!("no layers in manifest.json"))?;

        for layer_path in layers {
            let layer_file = layer_path
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("invalid layer path"))?;
            let layer_path = resolve_docker_save_file(&archive_root, layer_file)?;
            let data = std::fs::read(layer_path)
                .with_context(|| format!("failed to read layer {}", layer_file))?;

            let reader = spur_net::image_layer::decode(&data, None)
                .with_context(|| format!("failed to decode layer {}", layer_file))?;
            let mut archive = tar::Archive::new(reader);
            archive.set_overwrite(true);
            archive
                .unpack(dest)
                .with_context(|| format!("failed to extract layer {}", layer_file))?;
        }

        // Record the image config so container jobs can start from its config.Env.
        // Best effort: an archive without a readable config still imports.
        if let Some(config_file) = manifest
            .first()
            .and_then(|entry| entry.get("Config"))
            .and_then(|config| config.as_str())
        {
            let recorded = resolve_docker_save_file(&archive_root, config_file)
                .and_then(|path| std::fs::read(path).with_context(|| format!("read {config_file}")))
                .and_then(|config_json| spur_net::oci::record_image_config(dest, &config_json));
            if let Err(e) = recorded {
                eprintln!("warning: image environment not captured from {config_file}: {e:#}");
            }
        }

        Ok(())
    })();

    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn resolve_docker_save_file(archive_root: &Path, entry: &str) -> Result<PathBuf> {
    let relative = Path::new(entry);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("invalid docker save path: {entry}");
    }

    let resolved = archive_root
        .join(relative)
        .canonicalize()
        .with_context(|| format!("failed to resolve docker save path {entry}"))?;
    if !resolved.starts_with(archive_root) {
        bail!("docker save path escapes archive: {entry}");
    }
    if !resolved.is_file() {
        bail!("docker save path is not a regular file: {entry}");
    }

    Ok(resolved)
}

/// Pack a directory into a squashfs file.
fn pack_squashfs(rootfs: &str, output: &str) -> Result<()> {
    let result = std::process::Command::new("mksquashfs")
        .args([rootfs, output, "-noappend", "-comp", "zstd", "-quiet"])
        .output();

    match result {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("mksquashfs failed: {}", stderr.trim())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "mksquashfs not found. Install squashfs-tools:\n  \
                 sudo apt install squashfs-tools    # Debian/Ubuntu\n  \
                 sudo dnf install squashfs-tools    # Fedora/RHEL"
            )
        }
        Err(e) => bail!("failed to run mksquashfs: {}", e),
    }
}

fn cmd_list() -> Result<()> {
    let image_dir = resolve_image_dir();
    if !image_dir.exists() {
        eprintln!("No images imported yet.");
        return Ok(());
    }

    let mut images: Vec<(String, u64)> = Vec::new();
    for entry in std::fs::read_dir(image_dir)?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "sqsh") {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            images.push((name, size));
        }
    }

    if images.is_empty() {
        eprintln!("No images imported yet.");
        return Ok(());
    }

    images.sort_by(|a, b| a.0.cmp(&b.0));

    println!("{:<50} {:>10}", "IMAGE", "SIZE");
    for (name, size) in &images {
        let display_name = spur_net::oci::display_name(name);
        let size_str = if *size > 1_073_741_824 {
            format!("{:.1} GB", *size as f64 / 1_073_741_824.0)
        } else {
            format!("{:.1} MB", *size as f64 / 1_048_576.0)
        };
        println!("{:<50} {:>10}", display_name, size_str);
    }

    Ok(())
}

fn cmd_remove(name: &str) -> Result<()> {
    let sanitized = spur_net::oci::image_file_stem(name);
    let image_dir = resolve_image_dir();
    let path = image_dir.join(format!("{}.sqsh", sanitized));

    if !path.exists() {
        bail!("image '{}' not found", name);
    }

    std::fs::remove_file(&path)?;
    eprintln!("Removed: {}", name);
    Ok(())
}

/// Export a named container's rootfs back to a squashfs image.
fn cmd_export(name: &str, output: Option<&str>) -> Result<()> {
    let sanitized = spur_net::oci::sanitize_name(name);
    let container_dir = format!("/var/spool/spur/containers/{}", sanitized);

    if !std::path::Path::new(&container_dir).exists() {
        bail!(
            "container '{}' not found. Only named containers (--container-name) can be exported.",
            name
        );
    }

    let image_dir = resolve_image_dir();
    let output_path = output.map(|s| s.to_string()).unwrap_or_else(|| {
        image_dir
            .join(format!("{}.sqsh", sanitized))
            .to_string_lossy()
            .into()
    });

    eprintln!("Exporting container '{}' to {}", name, output_path);
    pack_squashfs(&container_dir, &output_path)?;

    let size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!(
        "Exported: {} ({:.1} MB)",
        output_path,
        size as f64 / 1_048_576.0
    );
    Ok(())
}

/// Resolve the image storage directory.
///
/// Priority:
/// 1. `$SPUR_IMAGE_DIR` environment variable
/// 2. `/var/spool/spur/images` if writable
/// 3. `~/.spur/images/` as user-local fallback
fn resolve_image_dir() -> std::path::PathBuf {
    // 1. Explicit env var
    if let Ok(dir) = std::env::var("SPUR_IMAGE_DIR") {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }

    // 2. System-wide default
    let system_dir = std::path::Path::new("/var/spool/spur/images");
    if is_dir_writable(system_dir) {
        return system_dir.to_path_buf();
    }

    // 3. User-local fallback
    if let Some(home) = std::env::var_os("HOME") {
        let user_dir = std::path::PathBuf::from(home).join(".spur/images");
        eprintln!(
            "Note: /var/spool/spur/images is not writable, using {}",
            user_dir.display()
        );
        return user_dir;
    }

    // Last resort: use system dir and let the error propagate at write time
    system_dir.to_path_buf()
}

/// Check if a directory exists and is writable, or if it can be created.
fn is_dir_writable(path: &std::path::Path) -> bool {
    if path.exists() {
        // Try creating a temp file to test writability
        let test_file = path.join(".spur_write_test");
        if std::fs::write(&test_file, b"").is_ok() {
            let _ = std::fs::remove_file(&test_file);
            return true;
        }
        false
    } else {
        // Check if parent is writable (so we can mkdir)
        path.parent()
            .map(|p| p.exists() && is_dir_writable(p))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{write::GzEncoder, Compression};

    use super::*;

    fn tar_files(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut archive = tar::Builder::new(&mut data);
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, path, *contents).unwrap();
        }
        archive.finish().unwrap();
        drop(archive);
        data
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn docker_manifest(layer_names: &[&str]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!([{
            "Config": "config.json",
            "RepoTags": ["fixture:latest"],
            "Layers": layer_names,
        }]))
        .unwrap()
    }

    fn docker_save_with_manifest(layer_names: &[&str], layers: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let manifest = docker_manifest(layer_names);
        let mut data = Vec::new();
        let mut archive = tar::Builder::new(&mut data);
        for (path, contents) in std::iter::once(("manifest.json", manifest.as_slice())).chain(
            layers
                .iter()
                .map(|(path, contents)| (*path, contents.as_slice())),
        ) {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, path, contents).unwrap();
        }
        archive.finish().unwrap();
        drop(archive);
        data
    }

    fn docker_save(layers: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let layer_names: Vec<&str> = layers.iter().map(|(name, _)| *name).collect();
        docker_save_with_manifest(&layer_names, layers)
    }

    #[cfg(unix)]
    fn docker_save_with_symlink_layer(layer_name: &str, target: &Path) -> Vec<u8> {
        let manifest = docker_manifest(&[layer_name]);
        let mut data = Vec::new();
        let mut archive = tar::Builder::new(&mut data);

        let mut manifest_header = tar::Header::new_gnu();
        manifest_header.set_size(manifest.len() as u64);
        manifest_header.set_mode(0o644);
        manifest_header.set_cksum();
        archive
            .append_data(&mut manifest_header, "manifest.json", manifest.as_slice())
            .unwrap();

        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::Symlink);
        link_header.set_size(0);
        link_header.set_mode(0o777);
        archive
            .append_link(&mut link_header, layer_name, target)
            .unwrap();

        archive.finish().unwrap();
        drop(archive);
        data
    }

    #[test]
    fn docker_save_import_supports_mixed_layer_compression() {
        let gzip = gzip(&tar_files(&[("gzip.txt", b"gzip"), ("order.txt", b"gzip")]));
        let zstd = zstd::stream::encode_all(
            tar_files(&[("zstd.txt", b"zstd"), ("order.txt", b"zstd")]).as_slice(),
            0,
        )
        .unwrap();
        let plain = tar_files(&[("plain.txt", b"plain"), ("order.txt", b"plain")]);
        let archive = docker_save(&[
            ("gzip/layer.tar", gzip),
            ("zstd/layer.tar", zstd),
            ("plain/layer.tar", plain),
        ]);
        let rootfs = tempfile::tempdir().unwrap();

        extract_docker_save_tar(&archive, rootfs.path().to_str().unwrap()).unwrap();

        assert_eq!(
            std::fs::read(rootfs.path().join("plain.txt")).unwrap(),
            b"plain"
        );
        assert_eq!(
            std::fs::read(rootfs.path().join("gzip.txt")).unwrap(),
            b"gzip"
        );
        assert_eq!(
            std::fs::read(rootfs.path().join("zstd.txt")).unwrap(),
            b"zstd"
        );
        assert_eq!(
            std::fs::read(rootfs.path().join("order.txt")).unwrap(),
            b"plain"
        );
    }

    #[test]
    fn docker_save_import_cleans_up_after_invalid_layer() {
        let archive =
            docker_save(&[("broken/layer.tar", vec![0x28, 0xb5, 0x2f, 0xfd, 0, 0, 0, 0])]);
        let rootfs = tempfile::tempdir().unwrap();

        let error = extract_docker_save_tar(&archive, rootfs.path().to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("broken/layer.tar"));
        assert!(!rootfs.path().join(".docker_save").exists());
    }

    #[test]
    fn docker_save_import_rejects_absolute_layer_path() {
        let base = tempfile::tempdir().unwrap();
        let rootfs = base.path().join("rootfs");
        let outside_layer = base.path().join("outside-layer.tar");
        let layer_data = tar_files(&[("escaped.txt", b"escaped")]);
        std::fs::write(&outside_layer, &layer_data).unwrap();
        let layer_name = outside_layer.to_str().unwrap();
        let archive = docker_save_with_manifest(&[layer_name], &[]);

        let error = extract_docker_save_tar(&archive, rootfs.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("invalid docker save path"));
        assert!(!rootfs.join("escaped.txt").exists());
        assert_eq!(std::fs::read(&outside_layer).unwrap(), layer_data);
        assert!(!rootfs.join(".docker_save").exists());
    }

    #[test]
    fn docker_save_import_rejects_parent_layer_path() {
        let base = tempfile::tempdir().unwrap();
        let rootfs = base.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let outside_layer = rootfs.join("outside-layer.tar");
        let layer_data = tar_files(&[("escaped.txt", b"escaped")]);
        std::fs::write(&outside_layer, &layer_data).unwrap();
        let archive = docker_save_with_manifest(&["../outside-layer.tar"], &[]);

        let error = extract_docker_save_tar(&archive, rootfs.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("invalid docker save path"));
        assert!(!rootfs.join("escaped.txt").exists());
        assert_eq!(std::fs::read(&outside_layer).unwrap(), layer_data);
        assert!(!rootfs.join(".docker_save").exists());
    }

    #[cfg(unix)]
    #[test]
    fn docker_save_import_rejects_symlinked_layer_path() {
        let base = tempfile::tempdir().unwrap();
        let rootfs = base.path().join("rootfs");
        let outside_layer = base.path().join("outside-layer.tar");
        let layer_data = tar_files(&[("escaped.txt", b"escaped")]);
        std::fs::write(&outside_layer, &layer_data).unwrap();
        let archive = docker_save_with_symlink_layer("layers/escape.tar", &outside_layer);

        let error = extract_docker_save_tar(&archive, rootfs.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("escapes archive"));
        assert!(!rootfs.join("escaped.txt").exists());
        assert_eq!(std::fs::read(&outside_layer).unwrap(), layer_data);
        assert!(!rootfs.join(".docker_save").exists());
    }
}
