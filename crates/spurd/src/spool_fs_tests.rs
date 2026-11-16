// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use nix::fcntl::openat;
use nix::sys::stat::{mkdirat, Mode};
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::process::Command;

fn run_child(mut command: Command, test: &str, marker: &str, value: &str, ignored: bool) {
    let (_, module) = module_path!().split_once("::").unwrap();
    let name = format!("{module}::{test}");
    command
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(marker, value);
    if ignored {
        command.arg("--ignored");
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("running 1 test"),
        "child {name} ({value}) failed: {}\n{stdout}\n{stderr}",
        output.status
    );
}

fn isolate(test: &str, marker: &str, value: &str, ignored: bool) {
    run_child(
        Command::new(std::env::current_exe().unwrap()),
        test,
        marker,
        value,
        ignored,
    );
}

fn deny_openat2(errno: i32) {
    let mut filter = [
        libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: libc::SYS_openat2 as u32,
        },
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ERRNO | errno as u32,
        },
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        },
    ];
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        0,
        "{}",
        io::Error::last_os_error()
    );
    assert_eq!(
        unsafe {
            libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
                0,
                0,
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    let anchor = File::open("/").unwrap();
    let denied = nix::fcntl::openat2(
        &anchor,
        Path::new("."),
        OpenHow::new().flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY),
    )
    .unwrap_err();
    assert_eq!(denied as i32, errno);
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("spool");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    (directory, root)
}

fn exercise_cleanup_safety() {
    let (directory, root) = fixture();
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), "untouched").unwrap();
    std::fs::create_dir_all(root.join("job1/nested")).unwrap();
    std::fs::write(root.join("job1/nested/output"), "stale output").unwrap();
    symlink(&outside, root.join("job1/outside-link")).unwrap();
    symlink("missing", root.join("job1/dangling-link")).unwrap();
    symlink(&outside, root.join("job2")).unwrap();
    let root_link = directory.path().join("spool-link");
    symlink(&root, &root_link).unwrap();

    assert!(cleanup_job_spool_under(&root_link, 1).is_err());
    assert!(open_directory(&root_link.join("job1")).is_err());
    assert!(root.join("job1/nested/output").is_file());
    let spool_root = SpoolRoot::open(root.clone()).unwrap();
    let candidates = spool_root.candidates().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].job_id, 1);
    assert!(!spool_root
        .held_identities(&candidates[0])
        .unwrap()
        .is_empty());
    cleanup_job_spool_under(&root, 1).unwrap();
    assert!(!root.join("job1").exists());
    assert!(root.join("job2").is_symlink());
    assert_eq!(
        std::fs::read_to_string(outside.join("sentinel")).unwrap(),
        "untouched"
    );
}

#[test]
fn cleanup_unlinks_symlinks_without_following_them() {
    exercise_cleanup_safety();
}

#[test]
fn cleanup_without_openat2() {
    const CHILD: &str = "SPUR_SPOOL_FS_COMPAT_ERRNO";
    let Some(errno) = std::env::var_os(CHILD) else {
        for errno in [libc::ENOSYS, libc::EPERM, libc::EINVAL] {
            isolate("cleanup_without_openat2", CHILD, &errno.to_string(), false);
        }
        return;
    };
    deny_openat2(errno.to_str().unwrap().parse().unwrap());
    exercise_cleanup_safety();
    child_names_cannot_escape_the_parent();
    cleanup_rejects_replaced_roots_and_jobs();
}

#[test]
fn cleanup_preserves_other_openat2_errors() {
    const CHILD: &str = "SPUR_SPOOL_FS_DENIED_ERRNO";
    let Some(errno) = std::env::var_os(CHILD) else {
        for errno in [libc::EACCES, libc::ELOOP, libc::EXDEV] {
            isolate(
                "cleanup_preserves_other_openat2_errors",
                CHILD,
                &errno.to_string(),
                false,
            );
        }
        return;
    };
    let errno = errno.to_str().unwrap().parse().unwrap();
    deny_openat2(errno);
    let (_directory, root) = fixture();
    std::fs::create_dir(root.join("job1")).unwrap();
    std::fs::write(root.join("job1/output"), "retained").unwrap();
    let error = cleanup_job_spool_under(&root, 1).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(errno));
    assert_eq!(
        std::fs::read_to_string(root.join("job1/output")).unwrap(),
        "retained"
    );
}

#[test]
fn child_names_cannot_escape_the_parent() {
    let (_directory, root) = fixture();
    std::fs::create_dir(root.join("nested")).unwrap();
    let parent = open_directory(&root).unwrap();
    for name in [
        "",
        "..",
        "../spool",
        "/",
        "nested/.",
        "nested/",
        "nested/..",
    ] {
        assert!(
            open_child(
                &parent,
                OsStr::new(name),
                OFlag::O_RDONLY | OFlag::O_DIRECTORY
            )
            .is_err(),
            "accepted invalid child name {name:?}"
        );
    }
    assert!(open_child(
        &parent,
        OsStr::new("."),
        OFlag::O_RDONLY | OFlag::O_DIRECTORY
    )
    .is_ok());
}

#[test]
fn cleanup_rejects_replaced_roots_and_jobs() {
    let (directory, root) = fixture();
    std::fs::create_dir(root.join("job1")).unwrap();
    std::fs::write(root.join("job1/output"), "original").unwrap();
    let spool_root = SpoolRoot::open(root.clone()).unwrap();
    let spool = spool_root.candidates().unwrap().pop().unwrap();
    std::fs::rename(root.join("job1"), root.join("retained-job")).unwrap();
    std::fs::create_dir(root.join("job1")).unwrap();
    std::fs::write(root.join("job1/output"), "replacement").unwrap();
    assert!(spool_root.delete(&spool).is_err());
    assert_eq!(
        std::fs::read_to_string(root.join("job1/output")).unwrap(),
        "replacement"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("retained-job/output")).unwrap(),
        "original"
    );

    let spool = spool_root.candidates().unwrap().pop().unwrap();
    let retained = directory.path().join("retained-root");
    std::fs::rename(&root, &retained).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(spool_root.delete(&spool).is_err());
    assert_eq!(
        std::fs::read_to_string(retained.join("job1/output")).unwrap(),
        "replacement"
    );
}

fn raw_directory(parent: &File, name: &str) -> nix::Result<File> {
    openat(
        parent,
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .map(Into::into)
}

struct DeepSpool {
    _directory: tempfile::TempDir,
    root: PathBuf,
    leaf_identity: (u64, u64),
}

impl DeepSpool {
    fn new(depth: usize) -> Self {
        let (directory, root) = fixture();
        std::fs::create_dir(root.join("job1")).unwrap();
        let mut fixture = Self {
            _directory: directory,
            root,
            leaf_identity: (0, 0),
        };
        let mut current = File::open(fixture.root.join("job1")).unwrap();
        for _ in 0..depth {
            mkdirat(&current, "d", Mode::from_bits_truncate(0o700)).unwrap();
            current = raw_directory(&current, "d").unwrap();
        }
        let mut leaf: File = openat(
            &current,
            "leaf",
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .unwrap()
        .into();
        leaf.write_all(b"retained until cleanup").unwrap();
        let metadata = leaf.metadata().unwrap();
        fixture.leaf_identity = (metadata.dev(), metadata.ino());
        fixture
    }
}

impl Drop for DeepSpool {
    fn drop(&mut self) {
        // Deep fixtures exceed PATH_MAX; clean them with a constant number of descriptors.
        let Ok(root) = File::open(&self.root) else {
            return;
        };
        let Ok(mut current) = raw_directory(&root, "job1") else {
            return;
        };
        let mut depth = 0;
        while let Ok(child) = raw_directory(&current, "d") {
            current = child;
            depth += 1;
        }
        let _ = unlinkat(&current, "leaf", UnlinkatFlags::NoRemoveDir);
        for _ in 0..depth {
            let Ok(parent) = raw_directory(&current, "..") else {
                return;
            };
            if unlinkat(&parent, "d", UnlinkatFlags::RemoveDir).is_err() {
                return;
            }
            current = parent;
        }
        let _ = unlinkat(&root, "job1", UnlinkatFlags::RemoveDir);
    }
}

fn set_fd_limit(soft: libc::rlim_t) {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    assert!(
        limit.rlim_max >= soft,
        "RLIMIT_NOFILE hard limit is below {soft}"
    );
    limit.rlim_cur = soft;
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
}

#[test]
fn traversal_fd_exhaustion_retains_the_spool() {
    const CHILD: &str = "SPUR_SPOOL_FS_LOW_FD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        isolate(
            "traversal_fd_exhaustion_retains_the_spool",
            CHILD,
            "1",
            false,
        );
        return;
    }
    let fixture = DeepSpool::new(128);
    let root = SpoolRoot::open(fixture.root.clone()).unwrap();
    let spool = root.candidates().unwrap().pop().unwrap();
    set_fd_limit(32);
    assert_eq!(
        root.held_identities(&spool).unwrap_err().raw_os_error(),
        Some(libc::EMFILE)
    );
    assert_eq!(
        root.delete(&spool).unwrap_err().raw_os_error(),
        Some(libc::EMFILE)
    );
    assert!(fixture.root.join("job1").is_dir());
}

#[test]
#[ignore = "requires an RLIMIT_NOFILE hard limit of at least 65536"]
fn inventory_and_cleanup_handle_six_thousand_levels() {
    const CHILD: &str = "SPUR_SPOOL_FS_DEEP_CHILD";
    let Some(mode) = std::env::var_os(CHILD) else {
        for mode in ["inventory", "delete"] {
            isolate(
                "inventory_and_cleanup_handle_six_thousand_levels",
                CHILD,
                mode,
                true,
            );
        }
        return;
    };
    set_fd_limit(65536);
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let fixture = DeepSpool::new(6000);
            let root = SpoolRoot::open(fixture.root.clone()).unwrap();
            let spool = root.candidates().unwrap().pop().unwrap();
            if mode == "inventory" {
                assert!(root
                    .held_identities(&spool)
                    .unwrap()
                    .contains(&fixture.leaf_identity));
            } else {
                assert_eq!(mode, "delete");
                cleanup_job_spool_under(&fixture.root, 1).unwrap();
                assert!(!fixture.root.join("job1").exists());
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

struct MountedPaths(Vec<PathBuf>);

impl Drop for MountedPaths {
    fn drop(&mut self) {
        for path in self.0.iter().rev() {
            let _ = nix::mount::umount2(path, nix::mount::MntFlags::MNT_DETACH);
        }
    }
}

#[test]
#[ignore = "requires unshare with user and mount namespaces"]
fn fallback_rejects_same_filesystem_bind_mounts() {
    const CHILD: &str = "SPUR_SPOOL_FS_MOUNT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut command = Command::new("unshare");
        command
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--propagation",
                "private",
            ])
            .arg(std::env::current_exe().unwrap());
        run_child(
            command,
            "fallback_rejects_same_filesystem_bind_mounts",
            CHILD,
            "1",
            true,
        );
        return;
    }
    let (directory, root) = fixture();
    let target = directory.path().join("outside");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("sentinel"), "untouched").unwrap();
    std::fs::create_dir(root.join("job1")).unwrap();
    std::fs::create_dir_all(root.join("job2/mounted")).unwrap();
    std::fs::create_dir(root.join("job3")).unwrap();
    let mut mounts = MountedPaths(Vec::new());
    for path in [root.join("job1"), root.join("job2/mounted")] {
        nix::mount::mount(
            Some(target.as_path()),
            &path,
            None::<&str>,
            nix::mount::MsFlags::MS_BIND,
            None::<&str>,
        )
        .unwrap();
        mounts.0.push(path.clone());
        assert_eq!(
            std::fs::metadata(&path).unwrap().dev(),
            std::fs::metadata(&root).unwrap().dev()
        );
    }
    deny_openat2(libc::ENOSYS);
    let spool_root = SpoolRoot::open(root.clone()).unwrap();
    let candidates = spool_root.candidates().unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|spool| spool.job_id)
            .collect::<std::collections::BTreeSet<_>>(),
        [2, 3].into_iter().collect()
    );
    let nested = candidates.iter().find(|spool| spool.job_id == 2).unwrap();
    assert_eq!(
        spool_root
            .held_identities(nested)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EXDEV)
    );
    for id in [1, 2] {
        assert_eq!(
            cleanup_job_spool_under(&root, id)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EXDEV)
        );
    }
    cleanup_job_spool_under(&root, 3).unwrap();
    assert_eq!(
        std::fs::read_to_string(target.join("sentinel")).unwrap(),
        "untouched"
    );
}
