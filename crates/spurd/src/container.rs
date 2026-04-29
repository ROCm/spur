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
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context};
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
///
/// Filters each candidate by `is_dir()` and dedupes. This function does no
/// I/O outside the existence check, takes no env or passwd dependencies, and
/// is the unit-testable core of `image_dirs_for_job`.
///
/// Order:
/// 1. `env_override` (no `is_dir()` filter — caller controls)
/// 2. `system_dir` if it exists
/// 3. `job_user_home/.spur/images` if it exists
/// 4. `agent_home/.spur/images` if it exists
///
/// If everything is filtered out, falls back to `system_dir` so callers
/// always get a non-empty list (used in error messages).
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
/// env var and the submitting user's personal image store.
///
/// The agent only needs *read* access to find images. We return all readable
/// dirs so images imported to either the system dir or a user's home dir are
/// found.
///
/// Search order:
/// 1. `$SPUR_IMAGE_DIR` environment variable
/// 2. `/var/spool/spur/images` (system-wide store)
/// 3. `~job_user/.spur/images` (submitting user's personal store)
/// 4. `$HOME/.spur/images` (agent process fallback)
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
#[derive(Debug)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    pub readonly: bool,
}

/// Container configuration for a job.
#[derive(Debug)]
pub struct ContainerConfig {
    pub image: String,
    pub mounts: Vec<BindMount>,
    pub workdir: Option<String>,
    pub name: Option<String>,
    pub readonly: bool,
    pub mount_home: bool,
    pub remap_root: bool,
    pub gpu_devices: Vec<u32>,
    pub environment: HashMap<String, String>,
    pub container_env: HashMap<String, String>,
    pub entrypoint: Option<String>,
    pub uid: u32,
    pub gid: u32,
    pub username: String,
    pub home_dir: String,
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
    let sanitized = sanitize_name(image);

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
#[derive(Debug, Clone, PartialEq)]
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
pub fn setup_rootfs(
    image_path: &Path,
    job_id: u32,
    name: Option<&str>,
) -> anyhow::Result<(PathBuf, RootfsMode)> {
    let cdir = container_dir();
    let base_dir = if let Some(name) = name {
        cdir.join(sanitize_name(name))
    } else {
        cdir.join(format!("job_{}", job_id))
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

/// Build the wrapper script that launches a job inside a container.
///
/// Implements Enroot-equivalent hooks:
/// - shadow: map host user into container /etc/passwd + /etc/group
/// - home: bind-mount user home directory
/// - devices: restrict /dev or passthrough GPU devices
/// - nvidia: bind-mount NVIDIA driver libs + devices
/// - rocm: bind-mount AMD ROCm libs + /dev/kfd + /dev/dri
/// - mellanox: bind-mount InfiniBand devices + MOFED libs
/// - cgroups: already handled by executor.rs
pub fn build_container_launch_script(
    config: &ContainerConfig,
    rootfs: &Path,
    inner_script_path: &str,
    job_id: u32,
) -> anyhow::Result<String> {
    let mut script = String::new();
    script.push_str("#!/bin/bash\nset -e\n\n");

    let rootfs_str = rootfs.to_string_lossy();

    // Ensure key directories exist in rootfs
    script.push_str(&format!(
        "mkdir -p {rootfs}/dev {rootfs}/proc {rootfs}/sys {rootfs}/tmp {rootfs}/etc {rootfs}/run\n",
        rootfs = rootfs_str
    ));

    // --- Hook: shadow (user mapping) ---
    // Map host user into container's /etc/passwd and /etc/group
    script.push_str(&format!(
        r#"
# Hook: shadow — map host user into container
if [ -f {rootfs}/etc/passwd ]; then
  grep -q "^{username}:" {rootfs}/etc/passwd 2>/dev/null || \
    echo "{username}:x:{uid}:{gid}::{home}:/bin/bash" >> {rootfs}/etc/passwd
fi
if [ -f {rootfs}/etc/group ]; then
  grep -q "^{username}:" {rootfs}/etc/group 2>/dev/null || \
    echo "{username}:x:{gid}:" >> {rootfs}/etc/group
fi
mkdir -p {rootfs}{home}
"#,
        rootfs = rootfs_str,
        username = config.username,
        uid = config.uid,
        gid = config.gid,
        home = config.home_dir,
    ));

    // Copy the job script into the rootfs
    let container_script = format!("{}/tmp/spur_job_{}.sh", rootfs_str, job_id);
    script.push_str(&format!(
        "cp \"{}\" \"{}\"\nchmod +x \"{}\"\n",
        inner_script_path, container_script, container_script
    ));

    // Pre-create mount targets
    for mount in &config.mounts {
        let target = format!("{}{}", rootfs_str, mount.target);
        script.push_str(&format!("mkdir -p \"{}\"\n", target));
    }

    // Build container-specific env exports
    let mut env_exports = String::new();
    // GPU visibility
    if !config.gpu_devices.is_empty() {
        let gpu_list: String = config
            .gpu_devices
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(",");
        env_exports.push_str(&format!(
            "export ROCR_VISIBLE_DEVICES={gl}\nexport CUDA_VISIBLE_DEVICES={gl}\nexport GPU_DEVICE_ORDINAL={gl}\n",
            gl = gpu_list
        ));
    }
    // User-specified container env vars (--container-env KEY=VAL)
    for (key, value) in &config.container_env {
        let escaped = value.replace('\'', "'\\''");
        env_exports.push_str(&format!("export {}='{}'\n", key, escaped));
    }

    let workdir = config.workdir.as_deref().unwrap_or("/tmp");

    // Build entrypoint prefix if specified
    let entrypoint_cmd = if let Some(ref ep) = config.entrypoint {
        format!("{} && ", ep)
    } else {
        String::new()
    };

    // === ROOT MODE: full namespace + chroot ===
    script.push_str(&format!(
        r#"
if [ "$(id -u)" = "0" ]; then
  exec unshare --mount --pid --fork bash -c '
set -e
ROOTFS="{rootfs}"

# Hook: filesystem mounts
mount -t proc proc $ROOTFS/proc 2>/dev/null || true
mount -t sysfs sys $ROOTFS/sys 2>/dev/null || true
mount -t tmpfs tmpfs $ROOTFS/run 2>/dev/null || true
"#,
        rootfs = rootfs_str,
    ));

    // Hook: devices — restricted /dev with only essential devices
    script.push_str(
        r#"
# Hook: devices — minimal /dev + GPU passthrough
mount -t tmpfs -o mode=755 tmpfs $ROOTFS/dev
mkdir -p $ROOTFS/dev/pts $ROOTFS/dev/shm $ROOTFS/dev/mqueue
mount -t devpts devpts $ROOTFS/dev/pts 2>/dev/null || true
mount -t tmpfs tmpfs $ROOTFS/dev/shm 2>/dev/null || true
# Essential devices
for d in null zero random urandom tty console; do
  touch $ROOTFS/dev/$d 2>/dev/null
  mount --bind /dev/$d $ROOTFS/dev/$d 2>/dev/null || true
done
ln -sf /proc/self/fd $ROOTFS/dev/fd 2>/dev/null || true
ln -sf /proc/self/fd/0 $ROOTFS/dev/stdin 2>/dev/null || true
ln -sf /proc/self/fd/1 $ROOTFS/dev/stdout 2>/dev/null || true
ln -sf /proc/self/fd/2 $ROOTFS/dev/stderr 2>/dev/null || true

# Hook: GPU — AMD (ROCm)
if [ -d /dev/dri ]; then
  mkdir -p $ROOTFS/dev/dri
  mount --bind /dev/dri $ROOTFS/dev/dri 2>/dev/null || true
fi
if [ -e /dev/kfd ]; then
  touch $ROOTFS/dev/kfd 2>/dev/null
  mount --bind /dev/kfd $ROOTFS/dev/kfd 2>/dev/null || true
fi
for p in /opt/rocm /opt/rocm/lib /opt/rocm/lib64; do
  if [ -d "$p" ]; then
    mkdir -p $ROOTFS$p
    mount --bind $p $ROOTFS$p 2>/dev/null || true
  fi
done

# Hook: GPU — NVIDIA
for dev in /dev/nvidia*; do
  if [ -e "$dev" ]; then
    touch $ROOTFS$dev 2>/dev/null || true
    mount --bind $dev $ROOTFS$dev 2>/dev/null || true
  fi
done
for libdir in /usr/lib/x86_64-linux-gnu /usr/lib64; do
  if ls $libdir/libnvidia* 1>/dev/null 2>&1; then
    mkdir -p $ROOTFS$libdir
    for lib in $libdir/libnvidia* $libdir/libcuda* $libdir/libnvoptix*; do
      [ -e "$lib" ] && mount --bind $lib $ROOTFS$lib 2>/dev/null || true
    done
  fi
done

# Hook: InfiniBand / Mellanox (MOFED)
if [ -d /dev/infiniband ]; then
  mkdir -p $ROOTFS/dev/infiniband
  mount --bind /dev/infiniband $ROOTFS/dev/infiniband 2>/dev/null || true
fi
for ibdev in /dev/uverbs* /dev/rdma_cm; do
  if [ -e "$ibdev" ]; then
    touch $ROOTFS$ibdev 2>/dev/null || true
    mount --bind $ibdev $ROOTFS$ibdev 2>/dev/null || true
  fi
done
for mofed in /etc/libibverbs.d /usr/lib/x86_64-linux-gnu/libibverbs /usr/lib64/libibverbs; do
  if [ -d "$mofed" ]; then
    mkdir -p $ROOTFS$mofed
    mount --bind $mofed $ROOTFS$mofed 2>/dev/null || true
  fi
done
"#,
    );

    // Hook: home — bind-mount user home directory
    if config.mount_home {
        script.push_str(&format!(
            r#"
# Hook: home — mount user home directory
mkdir -p $ROOTFS{home}
mount --bind {home} $ROOTFS{home} 2>/dev/null || true
"#,
            home = config.home_dir,
        ));
    }

    // User bind mounts
    for mount in &config.mounts {
        script.push_str(&format!(
            r#"
if [ -f "{source}" ]; then
  mkdir -p "$(dirname $ROOTFS{target})" && touch $ROOTFS{target}
else
  mkdir -p $ROOTFS{target}
fi
mount --bind "{source}" $ROOTFS{target} 2>/dev/null || true"#,
            source = mount.source,
            target = mount.target,
        ));
        if mount.readonly {
            script.push_str(&format!(
                "\nmount -o remount,bind,ro $ROOTFS{target} 2>/dev/null || true",
                target = mount.target,
            ));
        }
    }

    // Bind-mount host DNS files so name resolution works inside the container.
    script.push_str(
        r#"
for dns_file in /etc/resolv.conf /etc/hosts; do
  if [ -f "$dns_file" ]; then
    touch $ROOTFS$dns_file 2>/dev/null || true
    mount --bind "$dns_file" $ROOTFS$dns_file 2>/dev/null || true
  fi
done
"#,
    );

    // Hook: config.d — source any system/user hook scripts
    script.push_str(
        r#"

# Hook: config.d — run hook scripts from /etc/spur/container.d/hooks.d/
for hook in /etc/spur/container.d/hooks.d/*.sh; do
  [ -x "$hook" ] && ENROOT_ROOTFS=$ROOTFS ENROOT_PID=$$ . "$hook" 2>/dev/null || true
done

# Hook: environ.d — source extra environment files
for envf in /etc/spur/container.d/environ.d/*.env; do
  [ -f "$envf" ] && while IFS= read -r line; do
    [ -n "$line" ] && [ "${line#\#}" = "$line" ] && export "$line"
  done < "$envf"
done

# Hook: mounts.d — process extra mount specs
for fstab in /etc/spur/container.d/mounts.d/*.fstab; do
  [ -f "$fstab" ] && while IFS= read -r line; do
    [ -n "$line" ] && [ "${line#\#}" = "$line" ] && {
      src=$(echo "$line" | awk "{print \$1}")
      dst=$(echo "$line" | awk "{print \$2}")
      [ -n "$src" ] && [ -n "$dst" ] && {
        # Use touch for files, mkdir -p for directories — types must match.
        if [ -f "$src" ]; then
          mkdir -p "$(dirname $ROOTFS$dst)"
          touch $ROOTFS$dst 2>/dev/null || true
        else
          mkdir -p $ROOTFS$dst
        fi
        mount --bind "$src" $ROOTFS$dst 2>/dev/null || true
      }
    }
  done < "$fstab"
done
"#,
    );

    // Chroot and execute
    script.push_str(&format!(
        r#"
# Set environment
{env_exports}

# Enter container
chroot $ROOTFS /bin/bash -c "cd {workdir} && {entrypoint}{script}"
'
else
  # Non-root fallback: PATH-based execution, no namespace isolation
  ROOTFS="{rootfs}"
  {env_exports}
  export PATH="$ROOTFS/usr/bin:$ROOTFS/bin:$ROOTFS/usr/sbin:$ROOTFS/sbin:$PATH"
  export LD_LIBRARY_PATH="$ROOTFS/usr/lib:$ROOTFS/lib:$ROOTFS/usr/lib64:$ROOTFS/lib64:${{LD_LIBRARY_PATH:-}}"
  export SPUR_CONTAINER_ROOTFS="$ROOTFS"
  export HOME="{home}"
  cd "$ROOTFS{workdir}"
  {entrypoint}/bin/bash $ROOTFS/tmp/spur_job_{job_id}.sh
fi
"#,
        env_exports = env_exports,
        workdir = workdir,
        job_id = job_id,
        rootfs = rootfs_str,
        home = config.home_dir,
        entrypoint = entrypoint_cmd,
        script = format!("/tmp/spur_job_{}.sh", job_id),
    ));

    Ok(script)
}

/// Generate mount commands for GPU device passthrough.
fn build_gpu_mounts(config: &ContainerConfig, rootfs: &str) -> String {
    let mut script = String::new();

    // Always try to bind-mount /dev/dri if it exists (for GPU access)
    script.push_str(&format!(
        "if [ -d /dev/dri ]; then\n  mkdir -p {rootfs}/dev/dri\n  mount --bind /dev/dri {rootfs}/dev/dri\nfi\n",
        rootfs = rootfs
    ));

    // AMD: /dev/kfd is needed for ROCm
    script.push_str(&format!(
        "if [ -e /dev/kfd ]; then\n  touch {rootfs}/dev/kfd 2>/dev/null\n  mount --bind /dev/kfd {rootfs}/dev/kfd\nfi\n",
        rootfs = rootfs
    ));

    // AMD: bind-mount ROCm libraries if present
    for rocm_path in &["/opt/rocm", "/opt/rocm/lib"] {
        script.push_str(&format!(
            "if [ -d {rp} ]; then\n  mkdir -p {rootfs}{rp}\n  mount --bind {rp} {rootfs}{rp}\nfi\n",
            rp = rocm_path,
            rootfs = rootfs
        ));
    }

    // NVIDIA: bind-mount nvidia device files
    script.push_str(&format!(
        "for dev in /dev/nvidia*; do\n  if [ -e \"$dev\" ]; then\n    touch {rootfs}/$dev 2>/dev/null\n    mount --bind $dev {rootfs}/$dev\n  fi\ndone\n",
        rootfs = rootfs
    ));

    // NVIDIA: bind-mount driver libraries if present
    for nvidia_path in &[
        "/usr/lib/x86_64-linux-gnu/libnvidia",
        "/usr/lib64/libnvidia",
    ] {
        let dir = Path::new(nvidia_path)
            .parent()
            .unwrap_or(Path::new("/usr/lib"))
            .display();
        script.push_str(&format!(
            "if ls {np}* 1>/dev/null 2>&1; then\n  mkdir -p {rootfs}{dir}\n  for lib in {np}*; do\n    mount --bind $lib {rootfs}$lib\n  done\nfi\n",
            np = nvidia_path,
            rootfs = rootfs,
            dir = dir
        ));
    }

    // Set GPU visibility environment variables
    if !config.gpu_devices.is_empty() {
        let gpu_list: String = config
            .gpu_devices
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(",");
        script.push_str(&format!(
            "export ROCR_VISIBLE_DEVICES={gpu_list}\n\
             export CUDA_VISIBLE_DEVICES={gpu_list}\n\
             export GPU_DEVICE_ORDINAL={gpu_list}\n"
        ));
    }

    script
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
pub fn cleanup_rootfs(job_id: u32, mode: &RootfsMode) {
    let base_dir = container_dir().join(format!("job_{}", job_id));
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

/// Import a Docker/OCI image to squashfs format.
///
/// Uses spur-net's native OCI puller — downloads directly from registries
/// via the Docker Registry HTTP API v2. No dependency on Docker, skopeo,
/// umoci, or enroot. Only needs mksquashfs (squashfs-tools).
pub async fn import_image(uri: &str) -> anyhow::Result<PathBuf> {
    let dir = image_dir();
    spur_net::pull_image(uri, &dir).await
}

/// List imported images.
pub fn list_images() -> Vec<(String, u64)> {
    let dir = image_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let mut images = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "sqsh") {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                images.push((name, size));
            }
        }
    }
    images.sort_by(|a, b| a.0.cmp(&b.0));
    images
}

/// Remove an imported image.
pub fn remove_image(name: &str) -> anyhow::Result<()> {
    let path = image_dir().join(format!("{}.sqsh", sanitize_name(name)));
    if !path.exists() {
        bail!("image '{}' not found", name);
    }
    std::fs::remove_file(&path)?;
    info!(name, "image removed");
    Ok(())
}

/// Sanitize an image name for use as a filename.
fn sanitize_name(name: &str) -> String {
    name.replace("docker://", "")
        .replace('/', "+")
        .replace(':', "+")
}

/// Check if a binary is on PATH.
fn which(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    // --- Name sanitization ---

    #[test]
    fn test_sanitize_docker_uri() {
        assert_eq!(
            sanitize_name("docker://nvcr.io/nvidia/pytorch:24.01"),
            "nvcr.io+nvidia+pytorch+24.01"
        );
    }

    #[test]
    fn test_sanitize_simple_name() {
        assert_eq!(sanitize_name("ubuntu:22.04"), "ubuntu+22.04");
    }

    #[test]
    fn test_sanitize_nested_path() {
        assert_eq!(
            sanitize_name("registry.example.com/org/image:v1.2.3"),
            "registry.example.com+org+image+v1.2.3"
        );
    }

    #[test]
    fn test_sanitize_no_tag() {
        assert_eq!(sanitize_name("alpine"), "alpine");
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
        // Regression: error message now shows which directory was searched (#35).
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
        // Regression: agent used hardcoded /var/spool/spur/images ignoring env (#35 #23).
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
        // Regression: CLI used SPUR_IMAGE_DIR but agent did not (#35 #23).
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

    // --- #134: search submitting user's personal image store ---
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
        // End-to-end regression for #134: when the user's .spur/images
        // contains an image, resolve_image must find it via the
        // job_user_home path.
        //
        // Threads the synthetic home through compose_image_dirs by setting
        // SPUR_IMAGE_DIR. (The real production path goes uid -> passwd ->
        // home; that is covered separately by
        // test_resolve_job_user_home_for_current_uid +
        // test_compose_image_dirs_includes_existing_user_home_dir.)
        let images_dir = tempfile::tempdir().unwrap();
        let image_path = images_dir.path().join("myimage.sqsh");
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

    // --- GPU mounts ---

    #[test]
    fn test_gpu_mounts_with_devices() {
        let config = ContainerConfig {
            image: "test".into(),
            mounts: vec![],
            workdir: None,
            name: None,
            readonly: false,
            mount_home: false,
            remap_root: false,
            gpu_devices: vec![0, 1],
            environment: HashMap::new(),
            container_env: HashMap::new(),
            entrypoint: None,
            uid: 1000,
            gid: 1000,
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let script = build_gpu_mounts(&config, "/tmp/rootfs");
        // AMD devices
        assert!(script.contains("/dev/dri"));
        assert!(script.contains("/dev/kfd"));
        assert!(script.contains("/opt/rocm"));
        // NVIDIA devices
        assert!(script.contains("/dev/nvidia"));
        // Visibility env vars
        assert!(script.contains("ROCR_VISIBLE_DEVICES=0,1"));
        assert!(script.contains("CUDA_VISIBLE_DEVICES=0,1"));
    }

    #[test]
    fn test_gpu_mounts_no_devices() {
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
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let script = build_gpu_mounts(&config, "/tmp/rootfs");
        // Should still mount device dirs (if they exist on host)
        assert!(script.contains("/dev/dri"));
        // But no visibility env vars
        assert!(!script.contains("ROCR_VISIBLE_DEVICES"));
        assert!(!script.contains("CUDA_VISIBLE_DEVICES"));
    }

    // --- Container launch script ---

    #[test]
    fn test_launch_script_basic_structure() {
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
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 42).unwrap();

        assert!(script.starts_with("#!/bin/bash"));
        assert!(script.contains("set -e"));
        // Copies inner script into rootfs
        assert!(script.contains("/tmp/inner.sh"));
        assert!(script.contains("spur_job_42.sh"));
        // Has namespace/chroot logic
        assert!(script.contains("unshare"));
        assert!(script.contains("chroot"));
        // Non-root fallback
        assert!(script.contains("SPUR_CONTAINER_ROOTFS"));
    }

    #[test]
    fn test_launch_script_with_workdir() {
        let config = ContainerConfig {
            image: "test".into(),
            mounts: vec![],
            workdir: Some("/workspace".into()),
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
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();

        // Root branch: cd runs inside chroot so workdir is unqualified.
        assert!(script.contains(r#"cd /workspace &&"#));
        // Non-root fallback: no chroot, so cd must be prefixed with $ROOTFS
        // so it resolves against the container rootfs, not the host filesystem.
        // Regression test for: https://github.com/ROCm/spur/issues/136
        assert!(script.contains(r#"cd "$ROOTFS/workspace""#));
        // The old broken form must not appear in the non-root branch.
        assert!(!script.contains("\n  cd /workspace\n"));
    }

    #[test]
    fn test_launch_script_with_mounts() {
        let config = ContainerConfig {
            image: "test".into(),
            mounts: vec![
                BindMount {
                    source: "/data".into(),
                    target: "/mnt/data".into(),
                    readonly: true,
                },
                BindMount {
                    source: "/models".into(),
                    target: "/models".into(),
                    readonly: false,
                },
            ],
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
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();

        assert!(script.contains("mount --bind \"/data\""));
        assert!(script.contains("/mnt/data"));
        assert!(script.contains("remount,bind,ro"));
        assert!(script.contains("mount --bind \"/models\""));
        // The else branch for directory mounts must emit mkdir -p.
        assert!(script.contains("mkdir -p $ROOTFS/mnt/data"));
    }

    #[test]
    fn test_launch_script_file_mount_uses_touch_not_mkdir() {
        // File bind mounts must use touch, not mkdir -p — mismatched types fail silently.
        let config = ContainerConfig {
            image: "test".into(),
            mounts: vec![BindMount {
                source: "/etc/resolv.conf".into(),
                target: "/etc/resolv.conf".into(),
                readonly: true,
            }],
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
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();

        // The if branch must emit touch; the [ -f ] guard must be present.
        assert!(
            script.contains("touch $ROOTFS/etc/resolv.conf"),
            "file mount must use touch, got:\n{}",
            script
        );
        assert!(
            script.contains("if [ -f \"/etc/resolv.conf\" ]"),
            "file detection guard must be present"
        );
        assert!(script.contains("mount --bind \"/etc/resolv.conf\""));
    }

    #[test]
    fn test_launch_script_dns_files_always_mounted() {
        // DNS files must be auto-mounted even when the user specifies no mounts.
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
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();

        assert!(
            script.contains("/etc/resolv.conf"),
            "resolv.conf must be auto-mounted"
        );
        assert!(
            script.contains("/etc/hosts"),
            "/etc/hosts must be auto-mounted"
        );
        assert!(script.contains("touch $ROOTFS$dns_file"));
    }

    // --- Image removal ---

    #[test]
    fn test_remove_image_not_found() {
        let err = remove_image("nonexistent-image-that-doesnt-exist").unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "expected 'not found', got: {}",
            err
        );
    }

    // --- List images (empty) ---

    #[test]
    fn test_list_images_nonexistent_dir() {
        // Temporarily override — just test with a known empty path
        // list_images uses a hardcoded path, so this tests the "dir doesn't exist" case
        // by checking the function handles it gracefully
        let images = list_images();
        // May or may not have images depending on test env, but shouldn't panic
        let _ = images;
    }

    // --- Cleanup ---

    #[test]
    fn test_cleanup_rootfs_nonexistent() {
        // Should not panic when cleaning up a rootfs that doesn't exist
        cleanup_rootfs(999999, &RootfsMode::Extracted);
        cleanup_rootfs(999998, &RootfsMode::Overlay);
    }

    // --- Which ---

    #[test]
    fn test_which_finds_bash() {
        assert!(which("bash"));
    }

    #[test]
    fn test_which_not_found() {
        assert!(!which("nonexistent-binary-that-doesnt-exist-xyz"));
    }

    // --- New hook tests ---

    #[test]
    fn test_launch_script_has_shadow_hook() {
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
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();
        // Shadow hook: maps user into container
        assert!(script.contains("alice:x:1000:1000"));
        assert!(script.contains("/etc/passwd"));
        assert!(script.contains("/etc/group"));
    }

    #[test]
    fn test_launch_script_mount_home() {
        let config = ContainerConfig {
            image: "test".into(),
            mounts: vec![],
            workdir: None,
            name: None,
            readonly: false,
            mount_home: true,
            remap_root: false,
            gpu_devices: vec![],
            environment: HashMap::new(),
            container_env: HashMap::new(),
            entrypoint: None,
            uid: 1000,
            gid: 1000,
            username: "alice".into(),
            home_dir: "/home/alice".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();
        assert!(script.contains("mount --bind /home/alice"));
    }

    #[test]
    fn test_launch_script_no_mount_home_by_default() {
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
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();
        assert!(!script.contains("Hook: home"));
    }

    #[test]
    fn test_launch_script_has_infiniband_hook() {
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
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();
        assert!(script.contains("/dev/infiniband"));
        assert!(script.contains("libibverbs"));
    }

    #[test]
    fn test_launch_script_has_restricted_dev() {
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
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();
        // Restricted /dev with essential devices only
        assert!(script.contains("tmpfs tmpfs $ROOTFS/dev"));
        assert!(script.contains("null zero random urandom tty console"));
        assert!(script.contains("/dev/pts"));
        assert!(script.contains("/dev/shm"));
    }

    #[test]
    fn test_launch_script_container_env() {
        let mut container_env = HashMap::new();
        container_env.insert("MY_VAR".into(), "my_value".into());
        container_env.insert("ANOTHER".into(), "val2".into());
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
            container_env,
            entrypoint: None,
            uid: 1000,
            gid: 1000,
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();
        assert!(script.contains("MY_VAR='my_value'"));
        assert!(script.contains("ANOTHER='val2'"));
    }

    #[test]
    fn test_launch_script_entrypoint() {
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
            entrypoint: Some("/entrypoint.sh".into()),
            uid: 1000,
            gid: 1000,
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();
        assert!(script.contains("/entrypoint.sh &&"));
    }

    #[test]
    fn test_launch_script_config_hooks() {
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
            username: "testuser".into(),
            home_dir: "/home/testuser".into(),
        };
        let rootfs = Path::new("/tmp/test-rootfs");
        let script = build_container_launch_script(&config, rootfs, "/tmp/inner.sh", 1).unwrap();
        // Config hook directories
        assert!(script.contains("container.d/hooks.d"));
        assert!(script.contains("container.d/environ.d"));
        assert!(script.contains("container.d/mounts.d"));
    }
}
