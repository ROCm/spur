// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Native container support for Spur.
//!
//! Implements Enroot-like rootless containers using Linux user namespaces
//! and mount namespaces. No daemon, no Docker, no external runtime needed.
//!
//! Image format: squashfs (same as Enroot). Import OCI/Docker images with
//! `spur image import`.
//!
//! GPU passthrough:
//! - AMD: bind-mount /dev/kfd + /dev/dri/renderD* + ROCm libraries
//! - NVIDIA: bind-mount /dev/nvidia* + libnvidia-container or driver libs

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context};
use nix::mount::MsFlags;
use nix::sched::CloneFlags;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Where squashfs images and container rootfs are stored.
const DEFAULT_IMAGE_DIR: &str = "/var/spool/spur/images";
const DEFAULT_CONTAINER_DIR: &str = "/var/spool/spur/containers";

/// Resolve the home directory of the job's submitting user via passwd lookup.
fn resolve_job_user_home(job_user: Option<&str>, job_uid: Option<u32>) -> Option<PathBuf> {
    if let Some(uid) = job_uid {
        if let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
            return Some(user.dir);
        }
    }
    if let Some(name) = job_user {
        if let Ok(Some(user)) = nix::unistd::User::from_name(name) {
            return Some(user.dir);
        }
    }
    None
}

/// Pure composition of the image search path.
fn compose_image_dirs(
    env_override: Option<&Path>,
    system_dir: &Path,
    job_user_home: Option<&Path>,
    agent_home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    if let Some(d) = env_override {
        dirs.push(d.to_path_buf());
    }

    if system_dir.is_dir() && !dirs.iter().any(|x| x == system_dir) {
        dirs.push(system_dir.to_path_buf());
    }

    for home in [job_user_home, agent_home].into_iter().flatten() {
        let user_dir = home.join(".spur/images");
        if user_dir.is_dir() && !dirs.contains(&user_dir) {
            dirs.push(user_dir);
        }
    }

    if dirs.is_empty() {
        dirs.push(system_dir.to_path_buf());
    }
    dirs
}

/// Return candidate image directories for a job, honoring `SPUR_IMAGE_DIR`
/// and the submitting user's personal image store.
fn image_dirs_for_job(job_user: Option<&str>, job_uid: Option<u32>) -> Vec<PathBuf> {
    let env_override = std::env::var("SPUR_IMAGE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let job_user_home = resolve_job_user_home(job_user, job_uid);
    let agent_home = std::env::var_os("HOME").map(PathBuf::from);

    compose_image_dirs(
        env_override.as_deref(),
        Path::new(DEFAULT_IMAGE_DIR),
        job_user_home.as_deref(),
        agent_home.as_deref(),
    )
}

/// Primary image directory (first candidate) — used for error messages.
fn image_dir() -> PathBuf {
    image_dirs_for_job(None, None)
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_IMAGE_DIR))
}

/// Return the container rootfs directory, with user-local fallback.
///
/// Priority:
/// 1. `$SPUR_CONTAINER_DIR` environment variable
/// 2. `/var/spool/spur/containers` if writable
/// 3. `~/.spur/containers/` as user-local fallback
fn container_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SPUR_CONTAINER_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let system_dir = Path::new(DEFAULT_CONTAINER_DIR);
    // Check if system dir is writable (need to extract rootfs into it)
    if system_dir.is_dir() {
        let test_file = system_dir.join(".spur_write_test");
        if std::fs::write(&test_file, b"").is_ok() {
            let _ = std::fs::remove_file(&test_file);
            return system_dir.to_path_buf();
        }
    } else if std::fs::create_dir_all(system_dir).is_ok() {
        return system_dir.to_path_buf();
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".spur/containers");
    }
    system_dir.to_path_buf()
}

/// A parsed bind mount specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    pub readonly: bool,
}

/// Container configuration for a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    pub image: String,
    pub mounts: Vec<BindMount>,
    pub workdir: Option<String>,
    pub name: Option<String>,
    pub readonly: bool,
    pub mount_home: bool,
    pub remap_root: bool,
    pub gpu_devices: Vec<u64>,
    pub environment: HashMap<String, String>,
    pub container_env: HashMap<String, String>,
    pub entrypoint: Option<String>,
    pub uid: u32,
    pub gid: u32,
    pub username: String,
    pub home_dir: String,
    /// Registry-based device injection plan (replaces mount_hw_devices when present).
    pub device_plan: Option<spur_devices::inject::ContainerInjectionPlan>,
}

/// Bind-mount the host mint socket so nested `srun` can mint. Host UID 0 does
/// not receive the socket unless root jobs are explicitly allowed.
pub fn maybe_bind_auth_socket(
    mounts: &mut Vec<BindMount>,
    cluster_id: &str,
    uid: u32,
    allow_root_jobs: bool,
) {
    if cluster_id.is_empty() || (uid == 0 && !allow_root_jobs) {
        return;
    }
    let Ok(path) = spur_core::native_mint::resolve_socket_path(cluster_id) else {
        return;
    };
    if !path.exists() {
        return;
    }
    let displayed = path.display().to_string();
    if mounts.iter().any(|m| m.source == displayed) {
        return;
    }
    mounts.push(BindMount {
        source: displayed.clone(),
        target: displayed,
        readonly: false,
    });
}

/// Resolve image reference to a rootfs path.
///
/// Supports:
/// - Absolute path to squashfs file
/// - Image name (looked up in candidate image directories)
/// - docker:// URI (must be pre-imported with `spur image import`)
///
/// `job_user` / `job_uid` identify the submitting user so we can search
/// their personal image store (`~job_user/.spur/images`).
pub fn resolve_image(
    image: &str,
    job_user: Option<&str>,
    job_uid: Option<u32>,
) -> anyhow::Result<PathBuf> {
    let path = Path::new(image);

    // Absolute path: use directly if it exists
    if path.is_absolute() {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        // Path was resolved on the login node (sbatch) but doesn't exist
        // locally — try the basename in our local image directory. This
        // handles the case where login node and compute node use separate
        // (non-shared) image directories.
        if let Some(filename) = path.file_name() {
            let local = image_dir().join(filename);
            if local.exists() {
                return Ok(local);
            }
        }
    }

    let dirs = image_dirs_for_job(job_user, job_uid);
    let sanitized = spur_net::oci::image_file_stem(image);

    for dir in &dirs {
        // Try with .sqsh extension
        let image_path = dir.join(format!("{}.sqsh", sanitized));
        if image_path.exists() {
            return Ok(image_path);
        }
        // Try without extension
        let image_path = dir.join(&sanitized);
        if image_path.exists() {
            return Ok(image_path);
        }
    }

    let searched: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    bail!(
        "container image '{}' not found in [{}]. Import it first with: spur image import {}",
        image,
        searched.join(", "),
        image
    )
}

/// How the rootfs was set up — determines cleanup strategy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootfsMode {
    /// Extracted via unsquashfs — cleanup by removing the directory.
    Extracted,
    /// Mounted via squashfs + overlayfs — cleanup by unmounting.
    Overlay,
}

/// Create a container rootfs from a squashfs image.
///
/// Tries overlayfs mount first (fast, no disk copy) and falls back to
/// unsquashfs extraction if not root or mount fails.
///
/// Named containers always use extraction (they persist across jobs).
/// Rootfs directory base name for a batch job's container.
pub fn job_rootfs_base(job_id: u32) -> String {
    format!("job_{job_id}")
}

/// Rootfs directory base name for a per-step container. Kept in a separate
/// namespace from `job_rootfs_base` so a step can never resolve to (and later
/// delete) a batch job's live rootfs, and to avoid any id arithmetic.
pub fn step_rootfs_base(job_id: u32, step_id: u32) -> String {
    format!("step_{job_id}_{step_id}")
}

pub fn setup_rootfs(
    image_path: &Path,
    base: &str,
    name: Option<&str>,
) -> anyhow::Result<(PathBuf, RootfsMode)> {
    let cdir = container_dir();
    let base_dir = if let Some(name) = name {
        cdir.join(spur_net::oci::sanitize_name(name))
    } else {
        cdir.join(base)
    };

    // If named container already exists, reuse it
    if base_dir.exists() && name.is_some() {
        debug!(path = %base_dir.display(), "reusing named container");
        return Ok((base_dir, RootfsMode::Extracted));
    }

    // Try overlayfs mount first (unnamed containers only, requires root)
    if name.is_none() && nix::unistd::geteuid().is_root() {
        if let Ok(merged) = setup_rootfs_overlay(image_path, &base_dir) {
            return Ok((merged, RootfsMode::Overlay));
        }
        debug!("overlayfs mount failed, falling back to extraction");
    }

    // Fallback: extract with unsquashfs
    setup_rootfs_extract(image_path, &base_dir)?;
    Ok((base_dir, RootfsMode::Extracted))
}

/// Mount squashfs image read-only, then layer a tmpfs overlay on top.
///
/// Layout:
///   base_dir/lower   — squashfs mounted read-only
///   base_dir/upper   — tmpfs for writes
///   base_dir/work    — overlayfs workdir
///   base_dir/merged  — the merged rootfs (this is what gets chrooted)
fn setup_rootfs_overlay(image_path: &Path, base_dir: &Path) -> anyhow::Result<PathBuf> {
    let lower = base_dir.join("lower");
    let upper = base_dir.join("upper");
    let work = base_dir.join("work");
    let merged = base_dir.join("merged");

    for dir in [&lower, &upper, &work, &merged] {
        std::fs::create_dir_all(dir)?;
    }

    // Mount squashfs read-only
    let status = std::process::Command::new("mount")
        .args([
            "-t",
            "squashfs",
            "-o",
            "ro,loop",
            image_path.to_str().unwrap(),
            lower.to_str().unwrap(),
        ])
        .output()?;
    if !status.status.success() {
        let _ = std::fs::remove_dir_all(base_dir);
        bail!("failed to mount squashfs");
    }

    // Mount tmpfs for upper layer
    let status = std::process::Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs", upper.to_str().unwrap()])
        .output()?;
    if !status.status.success() {
        let _ = std::process::Command::new("umount").arg(&lower).output();
        let _ = std::fs::remove_dir_all(base_dir);
        bail!("failed to mount tmpfs for overlay upper");
    }

    // Mount overlayfs
    let overlay_opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    let status = std::process::Command::new("mount")
        .args([
            "-t",
            "overlay",
            "overlay",
            "-o",
            &overlay_opts,
            merged.to_str().unwrap(),
        ])
        .output()?;
    if !status.status.success() {
        let _ = std::process::Command::new("umount").arg(&upper).output();
        let _ = std::process::Command::new("umount").arg(&lower).output();
        let _ = std::fs::remove_dir_all(base_dir);
        bail!("failed to mount overlayfs");
    }

    info!(
        rootfs = %merged.display(),
        image = %image_path.display(),
        "container rootfs mounted (overlayfs)"
    );
    Ok(merged)
}

/// Extract squashfs image to a directory (fallback when overlayfs unavailable).
fn setup_rootfs_extract(image_path: &Path, rootfs: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(rootfs)
        .with_context(|| format!("failed to create container rootfs at {}", rootfs.display()))?;

    let unsquashfs_result = std::process::Command::new("unsquashfs")
        .args([
            "-f",
            "-d",
            rootfs.to_str().unwrap(),
            image_path.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();

    match unsquashfs_result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "unsquashfs failed for image {} (exit {}): {}",
                image_path.display(),
                output.status.code().unwrap_or(-1),
                stderr.trim()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "unsquashfs not found. Install squashfs-tools:\n  \
                 sudo apt install squashfs-tools    # Debian/Ubuntu\n  \
                 sudo dnf install squashfs-tools    # Fedora/RHEL"
            );
        }
        Err(e) => {
            bail!("failed to run unsquashfs: {}", e);
        }
    }

    info!(rootfs = %rootfs.display(), "container rootfs created (extracted)");
    Ok(())
}

/// Parse a bind mount spec like "/src:/dst:ro" into a BindMount.
pub fn parse_mount(spec: &str) -> anyhow::Result<BindMount> {
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.len() {
        2 => Ok(BindMount {
            source: parts[0].to_string(),
            target: parts[1].to_string(),
            readonly: false,
        }),
        3 => Ok(BindMount {
            source: parts[0].to_string(),
            target: parts[1].to_string(),
            readonly: parts[2].contains("ro"),
        }),
        _ => bail!("invalid mount spec '{}' — expected /src:/dst[:ro]", spec),
    }
}

/// Clean up an unnamed container rootfs.
///
/// Handles both overlay (unmount) and extracted (rm -rf) modes.
pub fn cleanup_rootfs(base: &str, mode: &RootfsMode) {
    let base_dir = container_dir().join(base);
    if !base_dir.exists() {
        return;
    }

    if *mode == RootfsMode::Overlay {
        // Unmount in reverse order: overlay, upper tmpfs, lower squashfs
        let merged = base_dir.join("merged");
        let upper = base_dir.join("upper");
        let lower = base_dir.join("lower");
        for mount_point in [&merged, &upper, &lower] {
            let _ = std::process::Command::new("umount")
                .arg(mount_point)
                .output();
        }
    }

    if let Err(e) = std::fs::remove_dir_all(&base_dir) {
        warn!(
            path = %base_dir.display(),
            error = %e,
            "failed to clean up container rootfs"
        );
    } else {
        debug!(path = %base_dir.display(), "container rootfs cleaned up");
    }
}

/// Creates a file or directory at the mount-point destination to match the
/// source type — bind mounts require the target to already exist. Returns a
/// handle to it, which a mount through [`fd_path`] cannot be redirected from.
pub fn create_mount_target(rootfs: &Path, target: &str, source: &Path) -> anyhow::Result<OwnedFd> {
    let root = crate::scratch::ScratchDir::open(rootfs)
        .with_context(|| format!("open container rootfs {}", rootfs.display()))?;
    let kind = if source.is_dir() {
        crate::scratch::MountPoint::Dir
    } else {
        crate::scratch::MountPoint::File
    };
    crate::scratch::open_mount_point(&root, Path::new(target), kind)
        .with_context(|| format!("creating mount target {target} in {}", rootfs.display()))
}

/// A path the kernel resolves to `point` itself, so a mount through it lands
/// there whatever has since been renamed or linked along the way it was found.
fn fd_path(point: &impl AsFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", point.as_fd().as_raw_fd()))
}

fn bind_mount(rootfs: &Path, source: &Path, target: &str, readonly: bool) -> anyhow::Result<()> {
    let point = create_mount_target(rootfs, target, source)?;
    nix::mount::mount(
        Some(source),
        &fd_path(&point),
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("bind mount {} -> {target}", source.display()))?;

    if readonly {
        // `point` still names what lies beneath the new mount; reopening the
        // target lands on the mount itself, which is what has to turn read-only.
        let mounted = create_mount_target(rootfs, target, source)?;
        nix::mount::mount(
            None::<&str>,
            &fd_path(&mounted),
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY,
            None::<&str>,
        )
        .with_context(|| format!("remount readonly {target}"))?;
    }
    Ok(())
}

/// Reopen `name` now that a mount sits on it. Anything renamed in under that name
/// since lives on `parent`'s own filesystem, so it is refused rather than filled.
fn open_fresh_mount(
    parent: &crate::scratch::ScratchDir,
    name: &str,
) -> std::io::Result<crate::scratch::ScratchDir> {
    let mounted = parent.subdir(std::ffi::OsStr::new(name), 0o755)?;
    if mounted.device()? == parent.device()? {
        return Err(std::io::Error::other(format!(
            "{} is not the mount just made",
            mounted.path().display()
        )));
    }
    Ok(mounted)
}

/// The folders the container's pseudo-filesystems mount onto.
struct PseudoFsDirs {
    root: crate::scratch::ScratchDir,
    proc: crate::scratch::ScratchDir,
    sys: crate::scratch::ScratchDir,
    dev: crate::scratch::ScratchDir,
    run: crate::scratch::ScratchDir,
}

/// The image is the user's, so a linked one of these refuses the container
/// rather than letting root create or mount through the link.
fn prepare_pseudo_fs_dirs(rootfs: &Path) -> anyhow::Result<PseudoFsDirs> {
    let root = crate::scratch::ScratchDir::open(rootfs)
        .with_context(|| format!("open container rootfs {}", rootfs.display()))?;
    let prepare = |name: &str, mode: u32| {
        root.subdir(std::ffi::OsStr::new(name), mode)
            .with_context(|| format!("prepare /{name} in the container rootfs"))
    };
    let (proc, sys, dev, run) = (
        prepare("proc", 0o755)?,
        prepare("sys", 0o755)?,
        prepare("dev", 0o755)?,
        prepare("run", 0o755)?,
    );
    prepare("tmp", 0o1777)?;
    prepare("etc", 0o755)?;
    Ok(PseudoFsDirs {
        root,
        proc,
        sys,
        dev,
        run,
    })
}

/// Set up /proc, /sys, /dev, and /run inside the container rootfs.
pub fn mount_filesystems(rootfs: &Path) -> anyhow::Result<()> {
    let dirs = prepare_pseudo_fs_dirs(rootfs)?;

    // /proc
    nix::mount::mount(
        Some("proc"),
        &fd_path(&dirs.proc),
        Some("proc"),
        MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        None::<&str>,
    )
    .context("mount proc")?;

    // /sys (read-only)
    nix::mount::mount(
        Some("sysfs"),
        &fd_path(&dirs.sys),
        Some("sysfs"),
        MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .unwrap_or_else(|e| warn!(error = %e, "mount sysfs (non-critical)"));

    // /dev (tmpfs, mode=755)
    nix::mount::mount(
        Some("tmpfs"),
        &fd_path(&dirs.dev),
        Some("tmpfs"),
        MsFlags::MS_NOEXEC | MsFlags::MS_STRICTATIME,
        Some("mode=755"),
    )
    .context("mount /dev tmpfs")?;
    let dev = open_fresh_mount(&dirs.root, "dev").context("open the container's /dev tmpfs")?;
    populate_dev(&dev);

    // /run
    nix::mount::mount(
        Some("tmpfs"),
        &fd_path(&dirs.run),
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=755"),
    )
    .unwrap_or_else(|e| warn!(error = %e, "mount /run (non-critical)"));

    Ok(())
}

/// Only this mount namespace sees the fresh /dev tmpfs, so once it is held as
/// `dev`, nothing in it can have been planted or swapped.
fn populate_dev(dev: &crate::scratch::ScratchDir) {
    for (name, fstype, flags, data) in [
        (
            "pts",
            "devpts",
            MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID,
            "newinstance,ptmxmode=0666,mode=620",
        ),
        (
            "shm",
            "tmpfs",
            MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            "mode=1777,size=50%",
        ),
    ] {
        let mounted = dev
            .subdir(std::ffi::OsStr::new(name), 0o755)
            .map_err(anyhow::Error::from)
            .and_then(|dir| {
                Ok(nix::mount::mount(
                    Some(fstype),
                    &fd_path(&dir),
                    Some(fstype),
                    flags,
                    Some(data),
                )?)
            });
        if let Err(e) = mounted {
            warn!(error = %e, "mount /dev/{name} (non-critical)");
        }
    }

    // Essential device nodes — bind-mount from host
    for name in ["null", "zero", "full", "random", "urandom", "tty"] {
        let host = Path::new("/dev").join(name);
        if !host.exists() {
            continue;
        }
        let bound = crate::scratch::open_mount_point(
            dev,
            Path::new(name),
            crate::scratch::MountPoint::File,
        )
        .map_err(anyhow::Error::from)
        .and_then(|node| {
            Ok(nix::mount::mount(
                Some(&host),
                &fd_path(&node),
                None::<&str>,
                MsFlags::MS_BIND,
                None::<&str>,
            )?)
        });
        if let Err(e) = bound {
            warn!(dev = name, error = %e, "bind mount /dev device");
        }
    }

    // /dev/console — only if a tty is attached
    crate::scratch::open_mount_point(dev, Path::new("console"), crate::scratch::MountPoint::File)
        .ok();

    for (target, name) in [
        ("/proc/self/fd", "fd"),
        ("/proc/self/fd/0", "stdin"),
        ("/proc/self/fd/1", "stdout"),
        ("/proc/self/fd/2", "stderr"),
        ("/dev/pts/ptmx", "ptmx"),
    ] {
        dev.symlink(target, name).ok();
    }
}

/// Apply a registry-based container device injection plan.
fn apply_container_device_plan(rootfs: &Path, plan: &spur_devices::inject::ContainerInjectionPlan) {
    // Device nodes: bind-mount from host into container rootfs
    for dn in &plan.device_nodes {
        let host = Path::new(&dn.host_path);
        if !host.exists() {
            warn!(path = %dn.host_path, "injected device node does not exist on host, skipping");
            continue;
        }
        if let Err(e) = bind_mount(rootfs, host, &dn.container_path, false) {
            warn!(
                device = %dn.container_path,
                error = %e,
                "failed to bind mount injected device node"
            );
        }
    }

    // Mounts: bind-mount library paths etc.
    for m in &plan.mounts {
        let source = Path::new(&m.source);
        if !source.exists() {
            debug!(path = %m.source, "injected mount source does not exist, skipping");
            continue;
        }
        if let Err(e) = bind_mount(rootfs, source, &m.target, m.readonly) {
            warn!(
                mount = %m.target,
                error = %e,
                "failed to bind mount injected mount"
            );
        }
    }
}

/// Process user-specified `--container-mounts` with source-type detection.
pub fn mount_user_binds(rootfs: &Path, mounts: &[BindMount]) -> anyhow::Result<()> {
    for m in mounts {
        let source = Path::new(&m.source);
        bind_mount(rootfs, source, &m.target, m.readonly)
            .with_context(|| format!("user mount {}:{}", m.source, m.target))?;
    }
    Ok(())
}

/// Bind-mount user home directory into the rootfs.
pub fn mount_home(rootfs: &Path, home_dir: &str) -> anyhow::Result<()> {
    let source = Path::new(home_dir);
    if source.is_dir() {
        bind_mount(rootfs, source, home_dir, false)
            .with_context(|| format!("mount home {}", home_dir))?;
    }
    Ok(())
}

fn is_loopback_nameserver(ip: &str) -> bool {
    ip.starts_with("127.") || ip == "::1"
}

/// Strip loopback nameservers from resolv.conf since local stub resolvers
/// (systemd-resolved, dnsmasq) are unreachable after pivot_root.
fn build_container_resolv_conf() -> String {
    const FALLBACK_RESOLV_CONF: &str = "\
# Generated by spur: no usable host resolv.conf found\n\
nameserver 1.1.1.1\n\
nameserver 8.8.8.8\n";
    let etc_resolv = Path::new("/etc/resolv.conf");
    let resolved = std::fs::canonicalize(etc_resolv).unwrap_or_else(|_| etc_resolv.to_path_buf());
    let contents = match std::fs::read_to_string(&resolved) {
        Ok(c) if !c.trim().is_empty() => c,
        _ => return FALLBACK_RESOLV_CONF.to_string(),
    };

    let has_loopback = contents.lines().any(|line| {
        let line = line.trim();
        line.starts_with("nameserver")
            && line
                .split_whitespace()
                .nth(1)
                .is_some_and(is_loopback_nameserver)
    });

    if !has_loopback {
        return contents;
    }

    // Try systemd-resolved's upstream config which has the real nameservers.
    let upstream = Path::new("/run/systemd/resolve/resolv.conf");
    if let Ok(upstream_contents) = std::fs::read_to_string(upstream) {
        let upstream_has_loopback = upstream_contents.lines().any(|line| {
            let line = line.trim();
            line.starts_with("nameserver")
                && line
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(is_loopback_nameserver)
        });
        if !upstream_has_loopback {
            return upstream_contents;
        }
    }

    // Last resort: strip loopback entries; if nothing remains, add public DNS.
    let mut filtered: Vec<&str> = Vec::new();
    let mut has_nameserver = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("nameserver") {
            if let Some(ip) = trimmed.split_whitespace().nth(1) {
                if is_loopback_nameserver(ip) {
                    continue;
                }
                has_nameserver = true;
            }
        }
        filtered.push(line);
    }
    if !has_nameserver {
        return FALLBACK_RESOLV_CONF.to_string();
    }
    filtered.join("\n") + "\n"
}

/// Write a fresh container's entry script into its rootfs's `/tmp`, still
/// reachable after pivot_root. The rootfs comes from a user's image, or a named
/// container shared across jobs, so nothing in it may be followed.
pub(crate) fn write_entry_script(
    rootfs: &Path,
    name: &str,
    content: &str,
) -> std::io::Result<PathBuf> {
    let tmp = crate::scratch::open_dir_beneath(rootfs, Path::new("tmp"), 0o1777)?;
    crate::scratch::write_scratch(
        &tmp,
        name,
        content.as_bytes(),
        crate::scratch::ScratchOwner::Daemon,
        0o755,
    )
}

/// The image is the user's, so a link at resolv.conf is replaced, not written through.
fn write_resolv_conf(rootfs: &Path, content: &str) -> std::io::Result<()> {
    let etc = crate::scratch::open_dir_beneath(rootfs, Path::new("etc"), 0o755)?;
    crate::scratch::write_scratch(
        &etc,
        "resolv.conf",
        content.as_bytes(),
        crate::scratch::ScratchOwner::Daemon,
        0o644,
    )?;
    Ok(())
}

/// These become whole lines of passwd and group, where a line break or ':'
/// would let the job write entries of its own choosing.
fn passwd_field_is_safe(value: &str) -> bool {
    !value.chars().any(|c| c == ':' || c.is_control())
}

/// Add `line` to `etc/<name>` unless it already lists `account`. The file is
/// replaced rather than appended to, so a link the image left there is never followed.
fn add_account_line(
    etc: &crate::scratch::ScratchDir,
    name: &str,
    account: &str,
    line: &str,
) -> anyhow::Result<()> {
    let content = match crate::scratch::read_scratch(etc, name) {
        Ok(content) => String::from_utf8_lossy(&content).into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("read the container's /etc/{name}"))
        }
    };
    if content
        .lines()
        .any(|existing| existing.starts_with(&format!("{account}:")))
    {
        return Ok(());
    }
    let mut updated = content;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(line);
    crate::scratch::write_scratch(
        etc,
        name,
        updated.as_bytes(),
        crate::scratch::ScratchOwner::Daemon,
        0o644,
    )
    .with_context(|| format!("write the container's /etc/{name}"))?;
    Ok(())
}

/// Inject DNS/NSS config into the container, skipping any the user already mounted.
pub fn mount_dns(rootfs: &Path, user_mounts: &[BindMount]) -> anyhow::Result<()> {
    let user_mounted = |target: &str| user_mounts.iter().any(|m| m.target == target);

    if !user_mounted("/etc/resolv.conf") {
        write_resolv_conf(rootfs, &build_container_resolv_conf())
            .unwrap_or_else(|e| warn!(error = %e, "failed to write container resolv.conf"));
    }

    for file in &["/etc/hosts", "/etc/nsswitch.conf"] {
        if !user_mounted(file) {
            let source = Path::new(file);
            if source.is_file() {
                bind_mount(rootfs, source, file, false).unwrap_or_else(|e| {
                    warn!(file, error = %e, "failed to mount DNS file");
                });
            }
        }
    }

    Ok(())
}

/// Map the host user into the container's /etc/passwd and /etc/group.
///
/// Only appends if the user doesn't already exist. Creates the home
/// directory inside the rootfs if it doesn't exist.
pub fn setup_shadow(rootfs: &Path, config: &ContainerConfig) -> anyhow::Result<()> {
    for (field, value) in [
        ("user name", &config.username),
        ("home directory", &config.home_dir),
    ] {
        if !passwd_field_is_safe(value) {
            bail!("container {field} {value:?} cannot appear in /etc/passwd");
        }
    }
    let etc = crate::scratch::open_dir_beneath(rootfs, Path::new("etc"), 0o755)
        .context("open the container's /etc")?;
    let user = &config.username;
    add_account_line(
        &etc,
        "passwd",
        user,
        &format!(
            "{user}:x:{}:{}::{}:/bin/bash\n",
            config.uid, config.gid, config.home_dir
        ),
    )?;
    add_account_line(&etc, "group", user, &format!("{user}:x:{}:\n", config.gid))?;

    // Ensure home directory exists inside rootfs
    crate::scratch::ScratchDir::open(rootfs)
        .and_then(|root| {
            crate::scratch::open_mount_point(
                &root,
                Path::new(&config.home_dir),
                crate::scratch::MountPoint::Dir,
            )
        })
        .ok();

    Ok(())
}

/// Execute admin hooks, parse environ.d, process mounts.d.
///
/// Returns additional environment variables from hook and environ.d files.
/// Hooks can inject env vars by appending KEY=VALUE lines to the file at
/// $SPUR_HOOK_ENVIRON (also available as $ENROOT_ENVIRON for compatibility).
pub fn run_hooks(rootfs: &Path) -> anyhow::Result<HashMap<String, String>> {
    run_hooks_from(Path::new("/etc/spur/container.d"), rootfs)
}

const HOOK_ENVIRON: &str = ".spur_hook_environ";

fn run_hooks_from(container_d: &Path, rootfs: &Path) -> anyhow::Result<HashMap<String, String>> {
    let mut extra_env = HashMap::new();

    let hooks_dir = container_d.join("hooks.d");
    if hooks_dir.is_dir() {
        // Shared env file for hooks to write KEY=VALUE lines into. Root's hooks
        // open it by name, so it lives where only root can replace it.
        let host_tmp = crate::scratch::ScratchDir::open(&std::env::temp_dir())
            .context("open the host temp directory for the hook environment")?;
        let env_name = format!("{HOOK_ENVIRON}.{}", uuid::Uuid::new_v4().simple());
        crate::scratch::open_scratch_output(
            &host_tmp,
            &env_name,
            crate::scratch::ScratchOwner::Daemon,
        )
        .context("create the hook environment file")?;
        let hook_env_file = host_tmp.join(&env_name);

        if let Ok(mut entries) = std::fs::read_dir(&hooks_dir) {
            let mut scripts: Vec<PathBuf> = Vec::new();
            while let Some(Ok(entry)) = entries.next() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "sh") {
                    scripts.push(path);
                }
            }
            scripts.sort();
            for script in scripts {
                debug!(hook = %script.display(), "running container hook");
                let status = std::process::Command::new("bash")
                    .arg(&script)
                    .env("ENROOT_ROOTFS", rootfs)
                    .env("ENROOT_PID", std::process::id().to_string())
                    .env("SPUR_HOOK_ENVIRON", &hook_env_file)
                    .env("ENROOT_ENVIRON", &hook_env_file)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                if let Err(e) = status {
                    warn!(hook = %script.display(), error = %e, "hook failed");
                }
            }
        }

        if let Ok(content) = crate::scratch::read_scratch(&host_tmp, &env_name) {
            for line in String::from_utf8_lossy(&content).lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    extra_env.insert(key.to_string(), value.to_string());
                }
            }
        }
        crate::scratch::unlink_scratch(&host_tmp, &env_name).ok();
    }

    // environ.d — parse KEY=VALUE lines and return them
    let environ_dir = container_d.join("environ.d");
    if environ_dir.is_dir() {
        if let Ok(mut entries) = std::fs::read_dir(&environ_dir) {
            let mut files: Vec<PathBuf> = Vec::new();
            while let Some(Ok(entry)) = entries.next() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "env") {
                    files.push(path);
                }
            }
            files.sort();
            for file in files {
                if let Ok(content) = std::fs::read_to_string(&file) {
                    for line in content.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            continue;
                        }
                        if let Some((key, value)) = line.split_once('=') {
                            extra_env.insert(key.to_string(), value.to_string());
                        }
                    }
                }
            }
        }
    }

    // mounts.d — parse fstab-style "src dst" lines, mount with source-type detection
    let mounts_dir = container_d.join("mounts.d");
    if mounts_dir.is_dir() {
        if let Ok(mut entries) = std::fs::read_dir(&mounts_dir) {
            let mut files: Vec<PathBuf> = Vec::new();
            while let Some(Ok(entry)) = entries.next() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "fstab") {
                    files.push(path);
                }
            }
            files.sort();
            for file in files {
                if let Ok(content) = std::fs::read_to_string(&file) {
                    for line in content.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            continue;
                        }
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 2 {
                            let src = Path::new(parts[0]);
                            let dst = parts[1];
                            if src.exists() {
                                bind_mount(rootfs, src, dst, false).unwrap_or_else(|e| {
                                    warn!(src = parts[0], dst, error = %e, "mounts.d entry failed");
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(extra_env)
}

/// pivot_root into the container rootfs — stronger than chroot since the
/// old root is unmounted, preventing escape via open file descriptors.
pub fn pivot_into_rootfs(rootfs: &Path, workdir: &str) -> anyhow::Result<()> {
    // Make rootfs a mount point (required by pivot_root)
    nix::mount::mount(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .context("bind mount rootfs onto itself")?;

    std::env::set_current_dir(rootfs).context("chdir to rootfs")?;

    nix::unistd::pivot_root(".", ".").context("pivot_root")?;

    // Unmount old root (stacked on top of new root after pivot)
    nix::mount::umount2(".", nix::mount::MntFlags::MNT_DETACH)
        .context("umount old root after pivot")?;

    // Set working directory
    std::env::set_current_dir(workdir)
        .or_else(|_| std::env::set_current_dir("/"))
        .context("chdir to workdir after pivot")?;

    Ok(())
}

/// Drop root privileges. Must call setgroups before setgid before setuid,
/// since each step requires the privilege dropped by the next.
pub fn drop_privileges(uid: u32, gid: u32, supplementary_gids: &[u32]) -> anyhow::Result<()> {
    if uid == 0 {
        return Ok(());
    }

    let gids: Vec<nix::unistd::Gid> = supplementary_gids
        .iter()
        .map(|g| nix::unistd::Gid::from_raw(*g))
        .collect();
    nix::unistd::setgroups(&gids).context("setgroups")?;
    nix::unistd::setgid(nix::unistd::Gid::from_raw(gid)).context("setgid")?;
    nix::unistd::setuid(nix::unistd::Uid::from_raw(uid)).context("setuid")?;

    Ok(())
}

fn merge_supplementary_gids(mut gids: Vec<u32>, extra: &[u32]) -> Vec<u32> {
    for &g in extra {
        if g > 0 && !gids.contains(&g) {
            gids.push(g);
        }
    }
    gids.sort_unstable();
    gids.dedup();
    gids
}

/// Collect the user's supplementary group IDs (e.g. video, render) from the
/// host. Must be called before fork while host /etc/group is still accessible.
pub fn resolve_supplementary_gids(uid: u32, gid: u32) -> Vec<u32> {
    let mut gids = vec![gid];

    let username = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name);

    if let Some(ref name) = username {
        if let Ok(content) = std::fs::read_to_string("/etc/group") {
            for line in content.lines() {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() >= 4 {
                    let group_gid: u32 = parts[2].parse().unwrap_or(0);
                    let members: Vec<&str> = parts[3].split(',').collect();
                    if members.contains(&name.as_str()) && !gids.contains(&group_gid) {
                        gids.push(group_gid);
                    }
                }
            }
        }
    }

    gids
}

/// Set up a user namespace for non-root container operation.
///
/// Maps the calling user to root inside the namespace, giving
/// CAP_SYS_ADMIN for mounts and pivot_root. Supplementary groups are
/// inherited from the parent process by writing `"deny"` to
/// `/proc/self/setgroups` (the crun/podman `keep-groups` approach).
/// The groups show as `nobody` inside the container but the kernel
/// still honours them for permission checks on device nodes.
fn setup_user_namespace(uid: u32, gid: u32) -> anyhow::Result<()> {
    nix::sched::unshare(
        CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID,
    )
    .context("unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWPID)")?;

    std::fs::write("/proc/self/uid_map", format!("0 {} 1\n", uid)).context("write uid_map")?;
    std::fs::write("/proc/self/setgroups", "deny").context("write setgroups deny")?;
    std::fs::write("/proc/self/gid_map", format!("0 {} 1\n", gid)).context("write gid_map")?;

    Ok(())
}

/// Make all mounts private so pivot_root works and mount/unmount events
/// don't propagate between the container and host.
fn set_mount_propagation_private() -> anyhow::Result<()> {
    nix::mount::mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .context("set mount propagation to private")
}

/// Fork to enter a new PID namespace. The child (PID 1 inside the
/// namespace) returns Ok(()); the parent waits for the child and exits.
fn fork_into_pid_namespace() -> anyhow::Result<()> {
    match unsafe { nix::unistd::fork().context("fork for PID namespace")? } {
        nix::unistd::ForkResult::Child => Ok(()),
        nix::unistd::ForkResult::Parent { child } => {
            let code = match nix::sys::wait::waitpid(child, None) {
                Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => code,
                _ => 1,
            };
            std::process::exit(code);
        }
    }
}

/// Close all inherited file descriptors except stdin/stdout/stderr
/// and the given preserve_fd (the sync pipe).
///
/// Prevents gRPC sockets, other jobs' output files, etc. from leaking
/// into the container process.
pub fn close_inherited_fds(preserve_fd: RawFd) {
    let fd_dir = Path::new("/proc/self/fd");
    // Collect fds first — iterating /proc/self/fd holds a directory fd
    // that we must not close while the iterator is alive.
    let fds: Vec<RawFd> = std::fs::read_dir(fd_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<RawFd>().ok())
        .filter(|&fd| fd > 2 && fd != preserve_fd)
        .collect();
    for fd in fds {
        unsafe {
            libc::close(fd);
        }
    }
}

/// The main child-process function for container setup.
pub fn container_init(
    config: &ContainerConfig,
    rootfs: &Path,
) -> anyhow::Result<HashMap<String, String>> {
    let is_root = nix::unistd::geteuid().is_root();

    // Resolve supplementary GIDs while host /etc/group is still accessible.
    let mut supplementary_gids = resolve_supplementary_gids(config.uid, config.gid);
    if is_root {
        if let Some(ref plan) = config.device_plan {
            supplementary_gids =
                merge_supplementary_gids(supplementary_gids, &plan.additional_gids);
        }
    }

    if is_root {
        nix::sched::unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID)
            .context("unshare(CLONE_NEWNS | CLONE_NEWPID)")?;
    } else {
        setup_user_namespace(config.uid, config.gid)
            .context("rootless container setup failed while setting up user namespace")?;
    }

    set_mount_propagation_private()?;
    fork_into_pid_namespace()?;

    mount_filesystems(rootfs)?;

    // Registry-based device injection
    if let Some(ref plan) = config.device_plan {
        apply_container_device_plan(rootfs, plan);
    }

    setup_shadow(rootfs, config)?;
    mount_user_binds(rootfs, &config.mounts)?;
    mount_dns(rootfs, &config.mounts)?;

    if config.mount_home {
        mount_home(rootfs, &config.home_dir)?;
    }

    let hook_env = run_hooks(rootfs)?;

    let workdir = config.workdir.as_deref().unwrap_or("/tmp");
    pivot_into_rootfs(rootfs, workdir)?;

    if is_root {
        drop_privileges(config.uid, config.gid, &supplementary_gids)?;
    }

    Ok(hook_env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_script_lands_in_the_rootfs_tmp() {
        use std::os::unix::fs::PermissionsExt;
        let rootfs = tempfile::tempdir().unwrap();
        std::fs::create_dir(rootfs.path().join("tmp")).unwrap();

        let path = write_entry_script(rootfs.path(), "spur_job_7.sh", "#!/bin/bash\ntrue\n")
            .expect("write entry script");

        assert_eq!(path, rootfs.path().join("tmp/spur_job_7.sh"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/bash\ntrue\n"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    // An image is user-supplied, so an absolute link inside it resolves against
    // the host when root writes before pivot_root.
    #[test]
    fn an_entry_script_never_follows_a_symlinked_tmp() {
        let rootfs = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(host.path(), rootfs.path().join("tmp")).unwrap();

        let error = write_entry_script(rootfs.path(), "spur_job_7.sh", "x")
            .expect_err("a linked /tmp must not be followed");

        assert!(
            matches!(
                error.raw_os_error(),
                Some(libc::ELOOP) | Some(libc::ENOTDIR)
            ),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(host.path()).unwrap().count(), 0);
    }

    // A named container's /tmp outlives the job that wrote to it, so anyone
    // who ran in it can leave a link at the next script's name.
    #[test]
    fn an_entry_script_replaces_a_link_planted_at_its_name() {
        let rootfs = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        let target = host.path().join("profile.sh");
        std::fs::write(&target, "ORIGINAL").unwrap();
        std::fs::create_dir(rootfs.path().join("tmp")).unwrap();
        std::os::unix::fs::symlink(&target, rootfs.path().join("tmp/spur_job_7.sh")).unwrap();

        let path = write_entry_script(rootfs.path(), "spur_job_7.sh", "#!/bin/bash\n")
            .expect("write entry script");

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ORIGINAL");
        assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
    }
    use std::sync::{Mutex, MutexGuard};

    /// Serialize tests that mutate `SPUR_IMAGE_DIR` (or any process-global
    /// env var). Cargo runs tests in parallel within a binary, so without
    /// a lock these races produce intermittent CI failures.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    // --- Mount parsing ---

    #[test]
    fn test_parse_mount_basic() {
        let m = parse_mount("/data:/data").unwrap();
        assert_eq!(m.source, "/data");
        assert_eq!(m.target, "/data");
        assert!(!m.readonly);
    }

    #[test]
    fn test_parse_mount_readonly() {
        let m = parse_mount("/src:/dst:ro").unwrap();
        assert_eq!(m.source, "/src");
        assert_eq!(m.target, "/dst");
        assert!(m.readonly);
    }

    #[test]
    fn test_parse_mount_rw_explicit() {
        let m = parse_mount("/src:/dst:rw").unwrap();
        assert!(!m.readonly);
    }

    #[test]
    fn test_parse_mount_one_part_fails() {
        let err = parse_mount("/only-one-part").unwrap_err();
        assert!(
            err.to_string().contains("invalid mount spec"),
            "expected 'invalid mount spec', got: {}",
            err
        );
        assert!(err.to_string().contains("/src:/dst"));
    }

    #[test]
    fn test_parse_mount_empty_fails() {
        assert!(parse_mount("").is_err());
    }

    #[test]
    fn test_parse_mount_too_many_parts_fails() {
        let err = parse_mount("/a:/b:ro:extra:parts").unwrap_err();
        assert!(err.to_string().contains("invalid mount spec"));
    }

    // --- Image resolution ---

    #[test]
    fn test_resolve_image_not_found() {
        let err = resolve_image("nonexistent-image-xyz", None, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not found"),
            "expected 'not found', got: {}",
            msg
        );
        assert!(
            msg.contains("spur image import"),
            "should suggest 'spur image import', got: {}",
            msg
        );
    }

    #[test]
    fn test_resolve_image_absolute_path_not_found() {
        let err = resolve_image("/nonexistent/path/to/image.sqsh", None, None).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_resolve_image_docker_uri_not_imported() {
        let err = resolve_image("docker://ubuntu:22.04", None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        assert!(msg.contains("spur image import"));
    }

    #[test]
    fn test_resolve_image_error_includes_directory() {
        // Regression: error message now shows which directory was searched.
        // Makes it obvious when CLI and agent use different directories.
        let err = resolve_image("missing-image", None, None).unwrap_err();
        let msg = err.to_string();
        // Error must tell user where we looked.
        assert!(
            msg.contains('/'),
            "error must include the directory searched, got: {}",
            msg
        );
    }

    #[test]
    fn test_image_dir_default_without_env() {
        // Regression: agent used hardcoded /var/spool/spur/images ignoring env.
        // Without SPUR_IMAGE_DIR the function must return the system default.
        let _guard = env_lock();
        let prev = std::env::var_os("SPUR_IMAGE_DIR");
        std::env::remove_var("SPUR_IMAGE_DIR");
        let dir = image_dir();
        if let Some(v) = prev {
            std::env::set_var("SPUR_IMAGE_DIR", v);
        }
        assert!(
            dir.to_str().unwrap().contains("spur"),
            "default image_dir must be under a spur path, got: {}",
            dir.display()
        );
    }

    #[test]
    fn test_image_dir_respects_spur_image_dir_env() {
        // Regression: CLI used SPUR_IMAGE_DIR but agent did not.
        // Both must use the same env var so images imported by non-root users
        // (to e.g. ~/.spur/images) are found by the agent.
        let _guard = env_lock();
        let prev = std::env::var_os("SPUR_IMAGE_DIR");
        std::env::set_var("SPUR_IMAGE_DIR", "/custom/image/store");
        let dir = image_dir();
        match prev {
            Some(v) => std::env::set_var("SPUR_IMAGE_DIR", v),
            None => std::env::remove_var("SPUR_IMAGE_DIR"),
        }
        assert_eq!(
            dir,
            std::path::PathBuf::from("/custom/image/store"),
            "SPUR_IMAGE_DIR env var must override the default image directory"
        );
    }

    #[test]
    fn test_image_dirs_for_job_no_user_falls_back() {
        let dirs = image_dirs_for_job(None, None);
        assert!(
            !dirs.is_empty(),
            "image_dirs_for_job(None, None) must return at least one directory"
        );
    }

    // --- search submitting user's personal image store ---
    //
    // Regression: spurd previously only searched `/var/spool/spur/images`.
    // When a user imported an image to `~/.spur/images` (no sudo),
    // dispatch failed with "image not found".
    //
    // These tests use the pure `compose_image_dirs` to exercise the
    // composition logic with synthetic paths, plus an end-to-end test
    // that creates a real on-disk fixture and verifies `resolve_image`
    // finds it.

    #[test]
    fn test_compose_image_dirs_includes_existing_user_home_dir() {
        // Regression core: when the job's user has ~/.spur/images, it's
        // added to the search path.
        let user_home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(user_home.path().join(".spur/images")).unwrap();

        let dirs = compose_image_dirs(
            None,
            Path::new("/__spur_test_no_such_dir__"), // system_dir doesn't exist
            Some(user_home.path()),
            None,
        );

        let expected = user_home.path().join(".spur/images");
        assert!(
            dirs.contains(&expected),
            "user .spur/images must be in dirs, got: {:?}",
            dirs
        );
    }

    #[test]
    fn test_compose_image_dirs_skips_user_home_when_no_images_dir() {
        // If the user's home exists but they haven't created .spur/images,
        // don't pollute the search list.
        let user_home = tempfile::tempdir().unwrap();
        // Note: NOT creating .spur/images

        let dirs = compose_image_dirs(
            None,
            Path::new("/__spur_test_no_such_dir__"),
            Some(user_home.path()),
            None,
        );

        let unexpected = user_home.path().join(".spur/images");
        assert!(
            !dirs.contains(&unexpected),
            "non-existent user dir must not be in list, got: {:?}",
            dirs
        );
    }

    #[test]
    fn test_compose_image_dirs_dedupes_user_home_against_agent_home() {
        // If the agent and the job user share the same HOME (e.g. agent
        // running as the user), don't list .spur/images twice.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".spur/images")).unwrap();

        let dirs = compose_image_dirs(
            None,
            Path::new("/__spur_test_no_such_dir__"),
            Some(home.path()),
            Some(home.path()),
        );

        let expected = home.path().join(".spur/images");
        let count = dirs.iter().filter(|d| **d == expected).count();
        assert_eq!(count, 1, "must dedupe identical user/agent homes");
    }

    #[test]
    fn test_compose_image_dirs_env_override_takes_precedence() {
        let env_dir = tempfile::tempdir().unwrap();
        let user_home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(user_home.path().join(".spur/images")).unwrap();

        let dirs = compose_image_dirs(
            Some(env_dir.path()),
            Path::new("/__spur_test_no_such_dir__"),
            Some(user_home.path()),
            None,
        );

        assert_eq!(
            dirs[0],
            env_dir.path().to_path_buf(),
            "env override must be first"
        );
    }

    #[test]
    fn test_compose_image_dirs_empty_falls_back_to_system() {
        // No env, no system dir, no user homes -> still get one entry
        // (the placeholder system dir) for error-message purposes.
        let dirs = compose_image_dirs(None, Path::new("/__spur_test_no_such_dir__"), None, None);
        assert_eq!(
            dirs,
            vec![PathBuf::from("/__spur_test_no_such_dir__")],
            "must fall back to system_dir even when nothing exists"
        );
    }

    #[test]
    fn test_resolve_job_user_home_for_current_uid() {
        // Proves the passwd lookup path works for a real uid. The current
        // process's uid must be resolvable.
        let uid = nix::unistd::Uid::current().as_raw();
        let home = resolve_job_user_home(None, Some(uid));
        assert!(home.is_some(), "current uid must resolve via passwd");
        let home = home.unwrap();
        assert!(home.is_absolute(), "home must be an absolute path");
    }

    #[test]
    fn test_resolve_job_user_home_invalid_uid_returns_none() {
        // A uid almost certainly absent from any passwd database.
        let home = resolve_job_user_home(None, Some(0xFFFF_FFFE));
        assert!(home.is_none(), "bogus uid must return None");
    }

    #[test]
    fn test_resolve_image_finds_image_in_user_home_dir() {
        // End-to-end regression: when the user's .spur/images
        // contains an image, resolve_image must find it via the
        // job_user_home path.
        //
        // Threads the synthetic home through compose_image_dirs by setting
        // SPUR_IMAGE_DIR. (The real production path goes uid -> passwd ->
        // home; that is covered separately by
        // test_resolve_job_user_home_for_current_uid +
        // test_compose_image_dirs_includes_existing_user_home_dir.)
        let images_dir = tempfile::tempdir().unwrap();
        // Stored under the canonical stem, matching how `spur image import`
        // names files (registry+repository+tag).
        let image_path = images_dir.path().join(format!(
            "{}.sqsh",
            spur_net::oci::image_file_stem("myimage")
        ));
        std::fs::write(&image_path, b"fake squashfs").unwrap();

        // Use SPUR_IMAGE_DIR to inject a known dir into the search path.
        // env_lock() serializes against other env-mutating tests in this
        // module, since cargo runs tests in parallel within a binary.
        let _guard = env_lock();
        let prev = std::env::var_os("SPUR_IMAGE_DIR");
        std::env::set_var("SPUR_IMAGE_DIR", images_dir.path());

        let result = resolve_image("myimage", None, None);

        // Restore env before asserting.
        match prev {
            Some(v) => std::env::set_var("SPUR_IMAGE_DIR", v),
            None => std::env::remove_var("SPUR_IMAGE_DIR"),
        }

        let resolved = result.expect("resolve_image must find myimage.sqsh");
        assert_eq!(resolved, image_path);
    }

    #[test]
    fn test_resolve_image_cross_form_reference() {
        let images_dir = tempfile::tempdir().unwrap();
        // Import under one form...
        let image_path = images_dir.path().join(format!(
            "{}.sqsh",
            spur_net::oci::image_file_stem("docker://busybox:latest")
        ));
        std::fs::write(&image_path, b"fake squashfs").unwrap();

        let _guard = env_lock();
        let prev = std::env::var_os("SPUR_IMAGE_DIR");
        std::env::set_var("SPUR_IMAGE_DIR", images_dir.path());

        // ...resolve under a different equivalent form.
        let result = resolve_image("busybox", None, None);

        match prev {
            Some(v) => std::env::set_var("SPUR_IMAGE_DIR", v),
            None => std::env::remove_var("SPUR_IMAGE_DIR"),
        }

        let resolved = result.expect(
            "resolve_image('busybox') must find image imported as 'docker://busybox:latest'",
        );
        assert_eq!(resolved, image_path);
    }

    // --- create_mount_target: source-type detection ---

    #[test]
    fn test_create_mount_target_file() {
        let rootfs = tempfile::tempdir().unwrap();
        let source = tempfile::NamedTempFile::new().unwrap();
        create_mount_target(rootfs.path(), "/etc/resolv.conf", source.path()).unwrap();
        let target = rootfs.path().join("etc/resolv.conf");
        assert!(target.exists(), "target file must exist");
        assert!(target.is_file(), "target must be a file, not directory");
    }

    #[test]
    fn test_create_mount_target_directory() {
        let rootfs = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        create_mount_target(rootfs.path(), "/mnt/data", source.path()).unwrap();
        let target = rootfs.path().join("mnt/data");
        assert!(target.exists(), "target dir must exist");
        assert!(target.is_dir(), "target must be a directory, not file");
    }

    #[test]
    fn test_create_mount_target_nested_file() {
        let rootfs = tempfile::tempdir().unwrap();
        let source = tempfile::NamedTempFile::new().unwrap();
        create_mount_target(rootfs.path(), "/deep/nested/path/file.conf", source.path()).unwrap();
        let target = rootfs.path().join("deep/nested/path/file.conf");
        assert!(
            target.is_file(),
            "deeply nested file target must be created"
        );
    }

    #[test]
    fn test_create_mount_target_fifo() {
        let rootfs = tempfile::tempdir().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("pipe");
        nix::unistd::mkfifo(
            &source,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();

        create_mount_target(rootfs.path(), "/dev/pipe", &source).unwrap();
        let target = rootfs.path().join("dev/pipe");
        assert!(target.exists(), "target file must exist for fifo source");
        assert!(target.is_file(), "target must be a file, not directory");
    }

    #[test]
    fn test_create_mount_target_symlink() {
        let rootfs = tempfile::tempdir().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let target_file = source_dir.path().join("real");
        std::fs::write(&target_file, "data").unwrap();
        let source = source_dir.path().join("link");
        std::os::unix::fs::symlink(&target_file, &source).unwrap();

        create_mount_target(rootfs.path(), "/dev/link", &source).unwrap();
        let target = rootfs.path().join("dev/link");
        assert!(target.exists(), "target file must exist for symlink source");
        assert!(target.is_file(), "target must be a file, not directory");
    }

    // The image is the user's, so its links are followed as the container will
    // see them: inside the rootfs, never out to the host.
    #[test]
    fn a_mount_target_behind_an_image_link_stays_inside_the_image() {
        let rootfs = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(host.path(), rootfs.path().join("mnt")).unwrap();

        create_mount_target(rootfs.path(), "/mnt/data", source.path()).unwrap();

        assert_eq!(std::fs::read_dir(host.path()).unwrap().count(), 0);
        let inside = rootfs.path().join(host.path().strip_prefix("/").unwrap());
        assert!(inside.join("data").is_dir());
    }

    #[test]
    fn a_file_mount_target_behind_an_image_link_stays_inside_the_image() {
        let rootfs = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        let target = host.path().join("nologin");
        std::fs::create_dir(rootfs.path().join("etc")).unwrap();
        std::os::unix::fs::symlink(&target, rootfs.path().join("etc/hosts")).unwrap();
        let source = tempfile::NamedTempFile::new().unwrap();

        create_mount_target(rootfs.path(), "/etc/hosts", source.path()).unwrap();

        assert!(!target.exists(), "no file may appear where the link points");
        assert!(rootfs
            .path()
            .join(target.strip_prefix("/").unwrap())
            .is_file());
    }

    #[test]
    fn a_mount_target_cannot_climb_out_of_the_rootfs() {
        let parent = tempfile::tempdir().unwrap();
        let rootfs = parent.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        let source = tempfile::tempdir().unwrap();

        create_mount_target(&rootfs, "/../escape", source.path()).unwrap();

        assert!(!parent.path().join("escape").exists());
        assert!(rootfs.join("escape").is_dir());
    }

    // Merged-/usr images reach /lib, /bin and /sbin through relative links, which
    // mounts beneath them must keep following.
    #[test]
    fn a_mount_target_follows_the_images_own_relative_links() {
        let rootfs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(rootfs.path().join("usr/lib")).unwrap();
        std::os::unix::fs::symlink("usr/lib", rootfs.path().join("lib")).unwrap();
        let source = tempfile::tempdir().unwrap();

        create_mount_target(rootfs.path(), "/lib/modules", source.path()).unwrap();

        assert!(rootfs.path().join("usr/lib/modules").is_dir());
    }

    #[test]
    fn a_folder_that_was_not_just_mounted_is_refused() {
        let rootfs = tempfile::tempdir().unwrap();
        std::fs::create_dir(rootfs.path().join("dev")).unwrap();
        let root = crate::scratch::ScratchDir::open(rootfs.path()).unwrap();

        assert!(open_fresh_mount(&root, "dev").is_err());
    }

    #[test]
    fn pseudo_filesystem_dirs_are_never_prepared_through_a_link() {
        let rootfs = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(host.path(), rootfs.path().join("run")).unwrap();

        assert!(prepare_pseudo_fs_dirs(rootfs.path()).is_err());
        assert_eq!(std::fs::read_dir(host.path()).unwrap().count(), 0);
    }

    #[test]
    fn pseudo_filesystem_dirs_are_created_when_the_image_lacks_them() {
        let rootfs = tempfile::tempdir().unwrap();
        prepare_pseudo_fs_dirs(rootfs.path()).unwrap();
        for dir in ["dev", "proc", "sys", "tmp", "etc", "run"] {
            assert!(rootfs.path().join(dir).is_dir(), "{dir}");
        }
    }

    #[test]
    fn resolv_conf_replaces_a_link_in_the_image() {
        let rootfs = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        let host_file = host.path().join("shadow");
        std::fs::write(&host_file, "ORIGINAL").unwrap();
        std::fs::create_dir(rootfs.path().join("etc")).unwrap();
        std::os::unix::fs::symlink(&host_file, rootfs.path().join("etc/resolv.conf")).unwrap();

        write_resolv_conf(rootfs.path(), "nameserver 10.0.0.1\n").unwrap();

        assert_eq!(std::fs::read_to_string(&host_file).unwrap(), "ORIGINAL");
        assert_eq!(
            std::fs::read_to_string(rootfs.path().join("etc/resolv.conf")).unwrap(),
            "nameserver 10.0.0.1\n"
        );
    }

    // Hooks run as root and open their env file by name, so it must sit where
    // the job's user cannot swap it for a link, which the rootfs is not.
    #[test]
    fn the_hook_environ_file_lives_outside_the_rootfs() {
        let container_d = tempfile::tempdir().unwrap();
        std::fs::create_dir(container_d.path().join("hooks.d")).unwrap();
        std::fs::write(
            container_d.path().join("hooks.d/10-env.sh"),
            "echo FOO=bar >> \"$SPUR_HOOK_ENVIRON\"\n\
             echo SEEN=$SPUR_HOOK_ENVIRON >> \"$SPUR_HOOK_ENVIRON\"\n",
        )
        .unwrap();
        let rootfs = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        let host_file = host.path().join("shadow");
        std::fs::write(&host_file, "ORIGINAL").unwrap();
        std::fs::create_dir(rootfs.path().join("tmp")).unwrap();
        std::os::unix::fs::symlink(&host_file, rootfs.path().join("tmp/.spur_hook_environ"))
            .unwrap();

        let env = run_hooks_from(container_d.path(), rootfs.path()).unwrap();

        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
        let seen = Path::new(&env["SEEN"]);
        assert!(!seen.starts_with(rootfs.path()), "{}", seen.display());
        assert!(!seen.exists(), "the env file is removed once read");
        assert_eq!(std::fs::read_to_string(&host_file).unwrap(), "ORIGINAL");
    }

    fn shadow_config(username: &str, home_dir: &str) -> ContainerConfig {
        ContainerConfig {
            image: "test".into(),
            mounts: vec![],
            workdir: None,
            name: None,
            readonly: false,
            mount_home: false,
            remap_root: false,
            gpu_devices: vec![],
            environment: HashMap::new(),
            container_env: HashMap::new(),
            entrypoint: None,
            uid: 1000,
            gid: 1000,
            username: username.into(),
            home_dir: home_dir.into(),
            device_plan: None,
        }
    }

    #[test]
    fn account_files_are_never_appended_through_a_link() {
        let rootfs = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        let host_passwd = host.path().join("passwd");
        std::fs::write(&host_passwd, "root:x:0:0:root:/root:/bin/bash\n").unwrap();
        std::fs::create_dir(rootfs.path().join("etc")).unwrap();
        std::os::unix::fs::symlink(&host_passwd, rootfs.path().join("etc/passwd")).unwrap();

        assert!(setup_shadow(rootfs.path(), &shadow_config("alice", "/home/alice")).is_err());
        assert_eq!(
            std::fs::read_to_string(&host_passwd).unwrap(),
            "root:x:0:0:root:/root:/bin/bash\n"
        );
    }

    // These become whole lines of passwd and group, so a line break in them
    // would let the job write accounts of its own choosing.
    #[test]
    fn a_home_dir_that_would_break_the_passwd_line_is_rejected() {
        let rootfs = tempfile::tempdir().unwrap();
        let etc = rootfs.path().join("etc");
        std::fs::create_dir(&etc).unwrap();
        std::fs::write(etc.join("passwd"), "root:x:0:0:root:/root:/bin/bash\n").unwrap();
        let home = "/home/alice\nevil::0:0::/root:/bin/bash";

        assert!(setup_shadow(rootfs.path(), &shadow_config("alice", home)).is_err());
        assert_eq!(
            std::fs::read_to_string(etc.join("passwd")).unwrap(),
            "root:x:0:0:root:/root:/bin/bash\n"
        );
    }

    #[test]
    fn passwd_fields_cannot_carry_a_line_break_or_a_separator() {
        assert!(passwd_field_is_safe("/home/alice"));
        assert!(!passwd_field_is_safe("/home/alice\nevil"));
        assert!(!passwd_field_is_safe("/home/a:b"));
    }

    // --- setup_shadow ---

    #[test]
    fn test_setup_shadow_creates_user_entry() {
        let rootfs = tempfile::tempdir().unwrap();
        let etc = rootfs.path().join("etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("passwd"), "root:x:0:0:root:/root:/bin/bash\n").unwrap();
        std::fs::write(etc.join("group"), "root:x:0:\n").unwrap();

        let config = ContainerConfig {
            image: "test".into(),
            mounts: vec![],
            workdir: None,
            name: None,
            readonly: false,
            mount_home: false,
            remap_root: false,
            gpu_devices: vec![],
            environment: HashMap::new(),
            container_env: HashMap::new(),
            entrypoint: None,
            uid: 1000,
            gid: 1000,
            username: "alice".into(),
            home_dir: "/home/alice".into(),
            device_plan: None,
        };
        setup_shadow(rootfs.path(), &config).unwrap();

        let passwd = std::fs::read_to_string(etc.join("passwd")).unwrap();
        assert!(passwd.contains("alice:x:1000:1000::/home/alice:/bin/bash"));

        let group = std::fs::read_to_string(etc.join("group")).unwrap();
        assert!(group.contains("alice:x:1000:"));

        assert!(rootfs.path().join("home/alice").is_dir());
    }

    #[test]
    fn test_setup_shadow_idempotent() {
        let rootfs = tempfile::tempdir().unwrap();
        let etc = rootfs.path().join("etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(
            etc.join("passwd"),
            "alice:x:1000:1000::/home/alice:/bin/bash\n",
        )
        .unwrap();
        std::fs::write(etc.join("group"), "alice:x:1000:\n").unwrap();

        let config = ContainerConfig {
            image: "test".into(),
            mounts: vec![],
            workdir: None,
            name: None,
            readonly: false,
            mount_home: false,
            remap_root: false,
            gpu_devices: vec![],
            environment: HashMap::new(),
            container_env: HashMap::new(),
            entrypoint: None,
            uid: 1000,
            gid: 1000,
            username: "alice".into(),
            home_dir: "/home/alice".into(),
            device_plan: None,
        };
        setup_shadow(rootfs.path(), &config).unwrap();

        let passwd = std::fs::read_to_string(etc.join("passwd")).unwrap();
        assert_eq!(
            passwd.lines().filter(|l| l.starts_with("alice:")).count(),
            1,
            "should not duplicate user entry"
        );
    }

    // --- build_container_resolv_conf ---

    #[test]
    fn test_build_container_resolv_conf_not_empty() {
        let contents = build_container_resolv_conf();
        assert!(!contents.is_empty(), "resolv.conf must not be empty");
    }

    #[test]
    fn test_is_loopback_nameserver() {
        assert!(is_loopback_nameserver("127.0.0.1"));
        assert!(is_loopback_nameserver("127.0.0.53"));
        assert!(is_loopback_nameserver("127.0.1.1"));
        assert!(is_loopback_nameserver("::1"));
        assert!(!is_loopback_nameserver("8.8.8.8"));
        assert!(!is_loopback_nameserver("1.1.1.1"));
        assert!(!is_loopback_nameserver("203.0.113.1"));
    }

    #[test]
    fn test_merge_supplementary_gids() {
        assert_eq!(
            merge_supplementary_gids(vec![1000], &[44, 109]),
            vec![44, 109, 1000]
        );
        assert_eq!(
            merge_supplementary_gids(vec![1000, 44], &[44, 109]),
            vec![44, 109, 1000]
        );
    }

    // --- resolve_supplementary_gids ---

    #[test]
    fn test_resolve_supplementary_gids_includes_primary() {
        let gids = resolve_supplementary_gids(1000, 1000);
        assert!(
            gids.contains(&1000),
            "must include the primary GID, got: {:?}",
            gids
        );
    }

    // --- drop_privileges ---

    #[test]
    fn test_drop_privileges_noop_for_root() {
        assert!(drop_privileges(0, 0, &[]).is_ok());
    }

    // --- Cleanup ---

    #[test]
    fn test_cleanup_rootfs_nonexistent() {
        // Should not panic when cleaning up a rootfs that doesn't exist
        cleanup_rootfs(&job_rootfs_base(999999), &RootfsMode::Extracted);
        cleanup_rootfs(&job_rootfs_base(999998), &RootfsMode::Overlay);
    }

    #[test]
    fn step_and_job_rootfs_bases_never_overlap() {
        // A step must never resolve to a batch job's rootfs dir, and the naming
        // must not depend on id arithmetic that could overflow or alias.
        assert_ne!(job_rootfs_base(1), step_rootfs_base(1, 0));
        assert_ne!(job_rootfs_base(10000), step_rootfs_base(1, 0));
        // Distinct steps of a job never collide, including large step ids that a
        // `job_id * 10000 + step_id` scheme would have wrapped or aliased.
        assert_ne!(step_rootfs_base(1, 0), step_rootfs_base(1, 10000));
        assert_ne!(step_rootfs_base(1, 2), step_rootfs_base(2, 1));
        // No batch job id maps onto a step base regardless of magnitude.
        for job in [0u32, 1, 42, 429_500, u32::MAX] {
            assert!(!job_rootfs_base(job).starts_with("step_"));
        }
    }

    // --- run_hooks ---

    #[test]
    fn test_run_hooks_returns_empty_when_no_dirs() {
        let rootfs = tempfile::tempdir().unwrap();
        let env = run_hooks(rootfs.path()).unwrap();
        assert!(env.is_empty());
    }

    #[test]
    fn test_build_container_resolv_conf_strips_loopback() {
        let contents = build_container_resolv_conf();
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("nameserver") {
                if let Some(ip) = trimmed.split_whitespace().nth(1) {
                    assert!(
                        !is_loopback_nameserver(ip),
                        "loopback nameserver {ip} leaked into container resolv.conf"
                    );
                }
            }
        }
        assert!(
            contents.contains("nameserver"),
            "container resolv.conf has no nameservers at all: {contents}"
        );
    }

    #[test]
    fn test_mount_dns_skips_user_mounted_resolv_conf() {
        let rootfs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(rootfs.path().join("etc")).unwrap();
        let sentinel = "# user-provided resolv.conf\nnameserver 9.9.9.9\n";
        std::fs::write(rootfs.path().join("etc/resolv.conf"), sentinel).unwrap();

        let user_mounts = vec![BindMount {
            source: "/dev/null".into(),
            target: "/etc/resolv.conf".into(),
            readonly: false,
        }];
        mount_dns(rootfs.path(), &user_mounts).unwrap();

        let after = std::fs::read_to_string(rootfs.path().join("etc/resolv.conf")).unwrap();
        assert_eq!(
            after, sentinel,
            "mount_dns overwrote user-mounted resolv.conf"
        );
    }

    #[test]
    fn test_close_inherited_fds_preserves_target() {
        use std::os::unix::io::AsRawFd;

        fn fd_is_open(fd: RawFd) -> bool {
            unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
        }

        // Run in a forked child so we don't close the test runner's fds
        match unsafe { nix::unistd::fork().unwrap() } {
            nix::unistd::ForkResult::Child => {
                let f1 = std::fs::File::open("/dev/null").unwrap();
                let f2 = std::fs::File::open("/dev/null").unwrap();
                let preserve = std::fs::File::open("/dev/null").unwrap();
                let preserve_fd = preserve.as_raw_fd();
                let f1_fd = f1.as_raw_fd();
                let f2_fd = f2.as_raw_fd();

                std::mem::forget(f1);
                std::mem::forget(f2);
                std::mem::forget(preserve);

                close_inherited_fds(preserve_fd);

                let preserved_ok = fd_is_open(preserve_fd);
                let f1_closed = !fd_is_open(f1_fd);
                let f2_closed = !fd_is_open(f2_fd);

                if preserved_ok && f1_closed && f2_closed {
                    std::process::exit(0);
                } else {
                    std::process::exit(1);
                }
            }
            nix::unistd::ForkResult::Parent { child } => {
                let status = nix::sys::wait::waitpid(child, None).unwrap();
                assert!(
                    matches!(status, nix::sys::wait::WaitStatus::Exited(_, 0)),
                    "close_inherited_fds did not preserve/close correctly: {:?}",
                    status
                );
            }
        }
    }
}
