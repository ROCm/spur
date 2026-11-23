// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Checkpoint socket paths and directory hardening, shared by spurd and
//! spurstepd. Every segment is lstat'd, so a planted symlink or a directory
//! owned by another user is never reused for the socket.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use spur_core::step::StepId;

/// Default `run_root` for the checkpoint socket tree.
pub const RUN_ROOT: &str = "/run/spur";

/// Mode the `checkpoint` directory is always reset to, whether created fresh
/// or found loose: root-traversable (dir lookup needs `x`) but not listable
/// by anyone else, since socket names leak job/step identifiers.
pub const CHECKPOINT_DIR_MODE: u32 = 0o711;

/// Mode a freshly created `run_root` or `<cluster>` segment gets. An existing
/// one is validated but left alone — `<cluster>` is spurauthd's directory.
const PARENT_SEGMENT_MODE: u32 = 0o755;

/// `<run_root>/<cluster>/checkpoint`, after validating `cluster_name`. Does
/// not touch the filesystem.
pub fn checkpoint_dir(run_root: &Path, cluster_name: &str) -> io::Result<PathBuf> {
    let dir = run_root.join(cluster_name).join("checkpoint");
    spur_core::native_mint::validate_cluster_name(cluster_name)
        .map_err(|error| invalid_input(&dir, &error.to_string()))?;
    Ok(dir)
}

/// `<dir>/<job>.<attempt>.<step>.sock`. `step_id` renders as the plain
/// integer [`spur_core::step::StepId`] is (a `u32` alias), matching
/// `StepdStore::session_dir`'s `{step_id}` — a reserved step id (e.g.
/// `STEP_BATCH`) is not rewritten to its display name here.
pub fn socket_path(dir: &Path, job_id: u32, run_attempt: u32, step_id: StepId) -> PathBuf {
    dir.join(format!("{job_id}.{run_attempt}.{step_id}.sock"))
}

/// Creates whichever of `run_root`, `<cluster>`, and `checkpoint` are missing,
/// and validates every one of them (existing or just created): refuses a
/// symlink, a non-directory, or anything not owned by `owner_uid`. A fresh
/// `run_root`/`<cluster>` gets mode 0755; an existing `<cluster>` keeps
/// whatever mode it already has (spurauthd owns it). `checkpoint` is always
/// left at [`CHECKPOINT_DIR_MODE`], loose or not.
pub fn harden(run_root: &Path, cluster_name: &str, owner_uid: u32) -> io::Result<PathBuf> {
    let checkpoint = checkpoint_dir(run_root, cluster_name)?;
    let cluster_dir = run_root.join(cluster_name);

    ensure_segment(run_root, owner_uid, PARENT_SEGMENT_MODE, true)?;
    ensure_segment(&cluster_dir, owner_uid, PARENT_SEGMENT_MODE, true)?;
    ensure_segment(&checkpoint, owner_uid, CHECKPOINT_DIR_MODE, false)?;
    fs::set_permissions(&checkpoint, fs::Permissions::from_mode(CHECKPOINT_DIR_MODE))?;

    Ok(checkpoint)
}

/// spurstepd's pre-bind check of the socket's parent directory: lstat, must
/// be a real directory, owned by `owner_uid`, mode bits exactly
/// [`CHECKPOINT_DIR_MODE`]. Also rejects a group- or world-writable
/// `checkpoint` directory parent (the `<cluster>` segment), the same ancestor
/// TOCTOU `harden()` guards against, as long as it is reachable.
pub fn validate_socket_parent(dir: &Path, owner_uid: u32) -> io::Result<()> {
    let meta = fs::symlink_metadata(dir)?;
    check_owned_dir(dir, &meta, owner_uid)?;
    let mode = meta.mode() & 0o7777;
    if mode != CHECKPOINT_DIR_MODE {
        return Err(permission_denied(
            dir,
            &format!("has mode {mode:o}, want {CHECKPOINT_DIR_MODE:o}"),
        ));
    }
    if let Some(parent) = dir.parent() {
        let parent_meta = fs::symlink_metadata(parent)?;
        check_owned_dir(parent, &parent_meta, owner_uid)?;
        reject_group_or_other_writable(parent, &parent_meta)?;
    }
    Ok(())
}

/// Create `path` with `create_mode` if it does not exist; otherwise validate
/// what is already there. Never follows a symlink at `path`. A freshly
/// created segment is validated the same way an existing one would be, so a
/// fresh dir and a re-validated dir behave identically on every later start.
/// `reject_writable` additionally refuses an existing `run_root`/`<cluster>`
/// segment that is group- or world-writable, before any child beneath it is
/// touched -- a writable ancestor would otherwise let another user swap a
/// child out from under an ownership check already passed on it.
fn ensure_segment(
    path: &Path,
    owner_uid: u32,
    create_mode: u32,
    reject_writable: bool,
) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            check_owned_dir(path, &meta, owner_uid)?;
            if reject_writable {
                reject_group_or_other_writable(path, &meta)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            // create_dir honours the umask, so an explicit chmod is needed
            // to get exactly create_mode regardless of the caller's umask.
            fs::set_permissions(path, fs::Permissions::from_mode(create_mode))?;
            let meta = fs::symlink_metadata(path)?;
            check_owned_dir(path, &meta, owner_uid)?;
            if reject_writable {
                reject_group_or_other_writable(path, &meta)?;
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn check_owned_dir(path: &Path, meta: &fs::Metadata, owner_uid: u32) -> io::Result<()> {
    if meta.file_type().is_symlink() {
        return Err(permission_denied(path, "is a symlink"));
    }
    if !meta.is_dir() {
        return Err(permission_denied(path, "is not a directory"));
    }
    if meta.uid() != owner_uid {
        return Err(permission_denied(
            path,
            &format!("is owned by uid {}, want {owner_uid}", meta.uid()),
        ));
    }
    Ok(())
}

/// Bits that let a user other than the owner write a directory's contents.
/// The sticky bit does not change that (it only restricts unlink/rename to
/// the owner within the directory, not whether others may write into it), so
/// it is not treated as making an otherwise writable directory acceptable.
const GROUP_OR_OTHER_WRITABLE: u32 = 0o022;

fn reject_group_or_other_writable(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let mode = meta.mode() & 0o7777;
    if mode & GROUP_OR_OTHER_WRITABLE != 0 {
        return Err(permission_denied(
            path,
            &format!("has mode {mode:o}, which is group- or world-writable"),
        ));
    }
    Ok(())
}

fn invalid_input(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{}: {reason}", path.display()),
    )
}

fn permission_denied(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {reason}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn owner_uid() -> u32 {
        nix::unistd::getuid().as_raw()
    }

    #[test]
    fn harden_creates_every_segment_with_0711_on_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().join("run_root");

        let checkpoint = harden(&run_root, "clusterA", owner_uid()).unwrap();

        assert_eq!(checkpoint, run_root.join("clusterA").join("checkpoint"));
        assert!(run_root.is_dir());
        assert!(run_root.join("clusterA").is_dir());
        assert!(checkpoint.is_dir());
        assert_eq!(mode_of(&checkpoint), CHECKPOINT_DIR_MODE);
    }

    #[test]
    fn harden_twice_over_a_dir_it_created_itself_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().join("run_root");

        let first = harden(&run_root, "clusterA", owner_uid()).unwrap();
        let second = harden(&run_root, "clusterA", owner_uid())
            .expect("re-validating a dir this process itself created must succeed");

        assert_eq!(first, second);
        assert_eq!(mode_of(&second), CHECKPOINT_DIR_MODE);
    }

    #[test]
    fn harden_refuses_a_symlinked_segment() {
        for segment in ["run_root", "cluster", "checkpoint"] {
            let temp = tempfile::tempdir().unwrap();
            let elsewhere = tempfile::tempdir().unwrap();
            let run_root = temp.path().join("run_root");

            match segment {
                "run_root" => {
                    std::os::unix::fs::symlink(elsewhere.path(), &run_root).unwrap();
                }
                "cluster" => {
                    fs::create_dir(&run_root).unwrap();
                    std::os::unix::fs::symlink(elsewhere.path(), run_root.join("clusterA"))
                        .unwrap();
                }
                "checkpoint" => {
                    fs::create_dir(&run_root).unwrap();
                    fs::create_dir(run_root.join("clusterA")).unwrap();
                    std::os::unix::fs::symlink(
                        elsewhere.path(),
                        run_root.join("clusterA").join("checkpoint"),
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }

            let error = harden(&run_root, "clusterA", owner_uid())
                .expect_err(&format!("a symlinked {segment} must be refused"));
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        }
    }

    #[test]
    fn harden_refuses_a_segment_owned_by_another_uid() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().join("run_root");
        fs::create_dir_all(run_root.join("clusterA").join("checkpoint")).unwrap();

        let error = harden(&run_root, "clusterA", owner_uid() + 1)
            .expect_err("a segment owned by a different uid must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[test]
    fn harden_refuses_a_file_where_a_directory_belongs() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().join("run_root");
        fs::create_dir_all(run_root.join("clusterA")).unwrap();
        fs::write(run_root.join("clusterA").join("checkpoint"), b"not a dir").unwrap();

        let error = harden(&run_root, "clusterA", owner_uid())
            .expect_err("a plain file must not be accepted as the checkpoint directory");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[test]
    fn harden_resets_a_loose_checkpoint_mode() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().join("run_root");
        let checkpoint = run_root.join("clusterA").join("checkpoint");
        fs::create_dir_all(&checkpoint).unwrap();
        // create_dir_all honours the umask for every segment it creates, not
        // just the leaf; set the ancestors back to what a fresh harden() run
        // itself would have left them at, so only the checkpoint dir's own
        // loose mode is under test here.
        fs::set_permissions(&run_root, fs::Permissions::from_mode(PARENT_SEGMENT_MODE)).unwrap();
        fs::set_permissions(
            run_root.join("clusterA"),
            fs::Permissions::from_mode(PARENT_SEGMENT_MODE),
        )
        .unwrap();
        fs::set_permissions(&checkpoint, fs::Permissions::from_mode(0o777)).unwrap();

        harden(&run_root, "clusterA", owner_uid()).unwrap();

        assert_eq!(mode_of(&checkpoint), CHECKPOINT_DIR_MODE);
    }

    #[test]
    fn harden_leaves_an_existing_cluster_dirs_mode_alone() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().join("run_root");
        let cluster_dir = run_root.join("clusterA");
        fs::create_dir_all(&cluster_dir).unwrap();
        // create_dir_all honours the umask, so run_root needs an explicit,
        // non-writable mode of its own -- unrelated to cluster_dir's, which
        // this test is actually about.
        fs::set_permissions(&run_root, fs::Permissions::from_mode(PARENT_SEGMENT_MODE)).unwrap();
        fs::set_permissions(&cluster_dir, fs::Permissions::from_mode(0o700)).unwrap();

        harden(&run_root, "clusterA", owner_uid()).unwrap();

        assert_eq!(mode_of(&cluster_dir), 0o700);
    }

    #[test]
    fn harden_refuses_a_group_writable_cluster_dir() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().join("run_root");
        let cluster_dir = run_root.join("clusterA");
        fs::create_dir_all(&cluster_dir).unwrap();
        fs::set_permissions(&cluster_dir, fs::Permissions::from_mode(0o775)).unwrap();

        let error = harden(&run_root, "clusterA", owner_uid())
            .expect_err("a group-writable cluster dir must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[test]
    fn harden_refuses_a_world_writable_run_root() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().join("run_root");
        fs::create_dir(&run_root).unwrap();
        fs::set_permissions(&run_root, fs::Permissions::from_mode(0o757)).unwrap();

        let error = harden(&run_root, "clusterA", owner_uid())
            .expect_err("a world-writable run_root must be refused even with the sticky bit");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[test]
    fn harden_accepts_a_preexisting_0755_run_root_and_cluster_dir() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().join("run_root");
        let cluster_dir = run_root.join("clusterA");
        fs::create_dir_all(&cluster_dir).unwrap();
        fs::set_permissions(&run_root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&cluster_dir, fs::Permissions::from_mode(0o755)).unwrap();

        harden(&run_root, "clusterA", owner_uid())
            .expect("a 0755 run_root and cluster dir must be accepted");
    }

    #[test]
    fn validate_socket_parents_cluster_ancestor_rejects_a_writable_dir() {
        let temp = tempfile::tempdir().unwrap();
        let cluster_dir = temp.path().join("clusterA");
        let checkpoint = cluster_dir.join("checkpoint");
        fs::create_dir_all(&checkpoint).unwrap();
        fs::set_permissions(&checkpoint, fs::Permissions::from_mode(CHECKPOINT_DIR_MODE)).unwrap();
        fs::set_permissions(&cluster_dir, fs::Permissions::from_mode(0o777)).unwrap();

        let error = validate_socket_parent(&checkpoint, owner_uid())
            .expect_err("a writable cluster ancestor must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[test]
    fn checkpoint_dir_rejects_bad_cluster_names() {
        let run_root = Path::new("/run/spur");
        for bad in ["", "..", "a/b"] {
            let error = checkpoint_dir(run_root, bad)
                .expect_err(&format!("{bad:?} must be rejected as a cluster name"));
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        }
    }

    #[test]
    fn socket_path_formats_job_attempt_step() {
        let dir = Path::new("/run/spur/clusterA/checkpoint");

        assert_eq!(
            socket_path(dir, 7, 1, 3),
            dir.join("7.1.3.sock"),
            "a numbered srun step stays a plain integer"
        );
        assert_eq!(
            socket_path(dir, 7, 1, spur_core::step::STEP_BATCH),
            dir.join(format!("7.1.{}.sock", spur_core::step::STEP_BATCH)),
            "STEP_BATCH must render as StepId's own Display (the raw u32), \
             matching StepdStore::session_dir, not step_display_name's \"batch\""
        );
    }

    #[test]
    fn validate_socket_parent_rejects_wrong_mode_owner_and_symlink() {
        let temp = tempfile::tempdir().unwrap();
        // Stands in for the <cluster> dir ancestor; only its children are
        // under test here, so give it a well-formed mode of its own.
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(PARENT_SEGMENT_MODE)).unwrap();

        let good = temp.path().join("checkpoint");
        fs::create_dir(&good).unwrap();
        fs::set_permissions(&good, fs::Permissions::from_mode(CHECKPOINT_DIR_MODE)).unwrap();
        validate_socket_parent(&good, owner_uid()).expect("a correctly hardened dir validates");

        let loose = temp.path().join("loose");
        fs::create_dir(&loose).unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).unwrap();
        let error = validate_socket_parent(&loose, owner_uid())
            .expect_err("a non-0711 mode must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");

        let error = validate_socket_parent(&good, owner_uid() + 1)
            .expect_err("a different owner_uid must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");

        let elsewhere = tempfile::tempdir().unwrap();
        let linked = temp.path().join("linked");
        std::os::unix::fs::symlink(elsewhere.path(), &linked).unwrap();
        let error =
            validate_socket_parent(&linked, owner_uid()).expect_err("a symlink must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }
}
