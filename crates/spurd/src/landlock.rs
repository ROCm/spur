//! Landlock filesystem access control for bare-metal job isolation.
//!
//! Restricts filesystem access to the job's working directory, GPU device
//! files, and read-only system paths. Prevents jobs from reading other
//! users' data or modifying system files.
//!
//! Inspired by the AXIS sandbox (axis-sandbox/src/linux/landlock.rs).
//! Requires Linux kernel 5.13+ (Landlock ABI v1). Gracefully skips on
//! older kernels.
//!
//! NOT applied to container jobs — chroot already provides filesystem
//! restriction for those.

use std::path::Path;
use tracing::{debug, info};

// Landlock ABI constants (from linux/landlock.h)
const LANDLOCK_CREATE_RULESET: i64 = 444;
const LANDLOCK_ADD_RULE: i64 = 445;
const LANDLOCK_RESTRICT_SELF: i64 = 446;

// Access rights for files
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;

const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

/// All read-only access rights
const READ_ONLY: u64 =
    LANDLOCK_ACCESS_FS_EXECUTE | LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;

/// All read-write access rights
const READ_WRITE: u64 = LANDLOCK_ACCESS_FS_EXECUTE
    | LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_READ_FILE
    | LANDLOCK_ACCESS_FS_READ_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_CHAR
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SOCK
    | LANDLOCK_ACCESS_FS_MAKE_FIFO
    | LANDLOCK_ACCESS_FS_MAKE_BLOCK
    | LANDLOCK_ACCESS_FS_MAKE_SYM;

/// Landlock ruleset attribute (ABI v1)
#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

/// Landlock path beneath attribute
#[repr(C)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Apply Landlock filesystem restrictions for a bare-metal job.
///
/// - `work_dir`: read-write access (job's workspace)
/// - System paths (`/usr`, `/lib`, `/opt/rocm`, etc.): read-only
/// - GPU devices (`/dev/dri`, `/dev/kfd`): read-write
/// - Everything else: no access
///
/// Returns Ok(()) on success, Err on failure (non-fatal — caller should
/// continue without Landlock).
pub fn apply_landlock_rules(work_dir: &str) -> Result<(), String> {
    // Create ruleset
    let attr = LandlockRulesetAttr {
        handled_access_fs: READ_WRITE,
    };

    let ruleset_fd = unsafe {
        libc::syscall(
            LANDLOCK_CREATE_RULESET,
            &attr as *const LandlockRulesetAttr,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0u32,
        )
    };

    if ruleset_fd < 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!(
            "landlock_create_ruleset failed (kernel may not support Landlock): {err}"
        ));
    }
    let ruleset_fd = ruleset_fd as i32;

    // Add read-only system paths
    let read_only_paths = [
        "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/opt", "/proc", "/sys", "/run",
    ];
    for path in &read_only_paths {
        if Path::new(path).exists() {
            add_path_rule(ruleset_fd, path, READ_ONLY);
        }
    }

    // Add read-write paths
    let rw_paths = [work_dir, "/tmp", "/dev/shm", "/var/tmp"];
    for path in &rw_paths {
        if Path::new(path).exists() {
            add_path_rule(ruleset_fd, path, READ_WRITE);
        }
    }

    // GPU device access (read-write for ioctl)
    let gpu_paths = [
        "/dev/dri",
        "/dev/kfd",
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/pts",
    ];
    for path in &gpu_paths {
        if Path::new(path).exists() {
            add_path_rule(ruleset_fd, path, READ_WRITE);
        }
    }

    // NVIDIA devices
    for i in 0..16 {
        let dev = format!("/dev/nvidia{i}");
        if Path::new(&dev).exists() {
            add_path_rule(ruleset_fd, &dev, READ_WRITE);
        }
    }
    for dev in &["/dev/nvidiactl", "/dev/nvidia-uvm", "/dev/nvidia-uvm-tools"] {
        if Path::new(dev).exists() {
            add_path_rule(ruleset_fd, dev, READ_WRITE);
        }
    }

    // Apply: no new privileges required (already set by seccomp)
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }

    let ret = unsafe { libc::syscall(LANDLOCK_RESTRICT_SELF, ruleset_fd, 0u32) };
    unsafe { libc::close(ruleset_fd) };

    if ret < 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("landlock_restrict_self failed: {err}"));
    }

    info!(work_dir, "landlock: filesystem restrictions applied");
    Ok(())
}

/// Add a path rule to a Landlock ruleset.
fn add_path_rule(ruleset_fd: i32, path: &str, access: u64) {
    let c_path = match std::ffi::CString::new(path) {
        Ok(p) => p,
        Err(_) => return,
    };

    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        // Path not accessible — skip (best-effort mode)
        return;
    }

    let attr = LandlockPathBeneathAttr {
        allowed_access: access,
        parent_fd: fd,
    };

    let ret = unsafe {
        libc::syscall(
            LANDLOCK_ADD_RULE,
            ruleset_fd,
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const LandlockPathBeneathAttr,
            0u32,
        )
    };

    if ret < 0 {
        debug!(path, "landlock: failed to add rule (skipping)");
    }

    unsafe { libc::close(fd) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_only_covers_system_paths() {
        // Verify READ_ONLY includes execute + read
        assert!(READ_ONLY & LANDLOCK_ACCESS_FS_EXECUTE != 0);
        assert!(READ_ONLY & LANDLOCK_ACCESS_FS_READ_FILE != 0);
        assert!(READ_ONLY & LANDLOCK_ACCESS_FS_READ_DIR != 0);
        // But not write
        assert!(READ_ONLY & LANDLOCK_ACCESS_FS_WRITE_FILE == 0);
    }

    #[test]
    fn test_read_write_covers_all_operations() {
        assert!(READ_WRITE & LANDLOCK_ACCESS_FS_EXECUTE != 0);
        assert!(READ_WRITE & LANDLOCK_ACCESS_FS_WRITE_FILE != 0);
        assert!(READ_WRITE & LANDLOCK_ACCESS_FS_READ_FILE != 0);
        assert!(READ_WRITE & LANDLOCK_ACCESS_FS_MAKE_DIR != 0);
        assert!(READ_WRITE & LANDLOCK_ACCESS_FS_REMOVE_FILE != 0);
    }

    #[test]
    fn test_gpu_device_paths_reasonable() {
        // /dev/dri should exist on GPU systems
        // /dev/kfd should exist on AMD GPU systems
        // This test just verifies the constants are sane
        assert_eq!(LANDLOCK_RULE_PATH_BENEATH, 1);
    }
}
