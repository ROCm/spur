// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Files spurd writes for a job while it holds root: scripts, wrappers and step
//! output. Every name is resolved against an open directory handle, so a link
//! the job's user planted is replaced, never followed.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{openat, renameat, OFlag};
use nix::sys::stat::{fchmod, fstat, mkdirat, Mode};
use nix::unistd::{fchown, unlinkat, Gid, Uid, UnlinkatFlags};

/// An open directory scratch files are created in by name.
pub(crate) struct ScratchDir {
    fd: OwnedFd,
    path: PathBuf,
}

/// Who a scratch file belongs to once it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScratchOwner {
    /// Stays with the daemon, for files the daemon itself executes.
    Daemon,
    /// Handed to the job's user.
    User { uid: u32, gid: u32 },
}

impl ScratchDir {
    /// Open a directory the daemon chose itself, so a symlink there is honoured.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(path)?;
        Ok(Self {
            fd: dir.into(),
            path: path.to_path_buf(),
        })
    }

    /// Open `name` beneath this directory, creating it if absent, never through a symlink.
    pub(crate) fn subdir(&self, name: &OsStr, mode: u32) -> io::Result<Self> {
        match mkdirat(&self.fd, name, Mode::from_bits_truncate(mode)) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(errno) => return Err(errno.into()),
        }
        let fd = openat(
            &self.fd,
            name,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            fd,
            path: self.path.join(name),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub(crate) fn owner_uid(&self) -> io::Result<u32> {
        Ok(fstat(&self.fd)?.st_uid)
    }

    pub(crate) fn set_owner(&self, uid: u32, gid: u32) -> io::Result<()> {
        Ok(fchown(
            &self.fd,
            Some(Uid::from_raw(uid)),
            Some(Gid::from_raw(gid)),
        )?)
    }

    pub(crate) fn set_mode(&self, mode: u32) -> io::Result<()> {
        Ok(fchmod(&self.fd, Mode::from_bits_truncate(mode))?)
    }
}

/// Write `content` to `name` through a fresh file renamed into place, so a
/// process still reading the old file keeps it and a planted link is replaced.
pub(crate) fn write_scratch(
    dir: &ScratchDir,
    name: &str,
    content: &[u8],
    owner: ScratchOwner,
    mode: u32,
) -> io::Result<PathBuf> {
    let temporary = format!(".{name}.{}", uuid::Uuid::new_v4().simple());
    let file = create_exclusive(dir, &temporary, 0o600)?;
    let written = fill(file, content, owner, mode).and_then(|()| {
        match renameat(&dir.fd, temporary.as_str(), &dir.fd, name) {
            Ok(()) => Ok(()),
            Err(Errno::EISDIR | Errno::ENOTEMPTY | Errno::EEXIST) => Err(occupied(dir, name)),
            Err(errno) => Err(errno.into()),
        }
    });
    if written.is_err() {
        let _ = unlinkat(&dir.fd, temporary.as_str(), UnlinkatFlags::NoRemoveDir);
    }
    written.map(|()| dir.join(name))
}

/// Create `name` empty and private for a step's output. Whatever held the name
/// is unlinked first, so a planted symlink or hardlink is never written through.
pub(crate) fn open_scratch_output(
    dir: &ScratchDir,
    name: &str,
    owner: ScratchOwner,
) -> io::Result<File> {
    match unlinkat(&dir.fd, name, UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(Errno::ENOENT) => {}
        Err(Errno::EISDIR) => return Err(occupied(dir, name)),
        Err(errno) => return Err(errno.into()),
    }
    let file = match create_exclusive(dir, name, 0o600) {
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
            return Err(occupied(dir, name))
        }
        created => created?,
    };
    hand_over(&file, owner, 0o600)?;
    Ok(file)
}

pub(crate) fn read_scratch(dir: &ScratchDir, name: &str) -> io::Result<Vec<u8>> {
    let fd = openat(
        &dir.fd,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    let mut content = Vec::new();
    File::from(fd).read_to_end(&mut content)?;
    Ok(content)
}

pub(crate) fn unlink_scratch(dir: &ScratchDir, name: &str) -> io::Result<()> {
    match unlinkat(&dir.fd, name, UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(Errno::ENOENT) => Ok(()),
        Err(errno) => Err(errno.into()),
    }
}

/// Open `relative` beneath `root`, creating missing components and refusing any symlink.
pub(crate) fn open_dir_beneath(root: &Path, relative: &Path, mode: u32) -> io::Result<ScratchDir> {
    let mut dir = ScratchDir::open(root)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a plain relative path", relative.display()),
            ));
        };
        dir = dir.subdir(name, mode)?;
    }
    Ok(dir)
}

/// Whether a root daemon may take over an existing job spool folder: its own,
/// or one an older agent handed to the job. An unprivileged daemon can neither
/// chown a folder nor be escalated through one, so it takes whatever it opened.
pub(crate) fn may_claim_spool_dir(daemon_is_root: bool, owner_uid: u32, job_uid: u32) -> bool {
    !daemon_is_root || owner_uid == 0 || (job_uid != 0 && owner_uid == job_uid)
}

/// Open a spool file for reading without following a symlink at its name.
pub(crate) async fn open_spool_file(path: &Path) -> io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .await
}

fn create_exclusive(dir: &ScratchDir, name: &str, mode: u32) -> io::Result<File> {
    let fd = openat(
        &dir.fd,
        name,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(mode),
    )?;
    Ok(File::from(fd))
}

fn fill(mut file: File, content: &[u8], owner: ScratchOwner, mode: u32) -> io::Result<()> {
    file.write_all(content)?;
    hand_over(&file, owner, mode)
}

fn hand_over(file: &File, owner: ScratchOwner, mode: u32) -> io::Result<()> {
    if let ScratchOwner::User { uid, gid } = owner {
        std::os::unix::fs::fchown(file, Some(uid), Some(gid))?;
    }
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

/// Only a folder an older agent handed to the job can hold an entry spurd did
/// not create, so its blocking a name must not read as the node's own I/O failure.
fn occupied(dir: &ScratchDir, name: &str) -> io::Error {
    io::Error::other(format!(
        "{} is occupied by an entry spurd did not create",
        dir.join(name).display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(temp: &tempfile::TempDir) -> ScratchDir {
        ScratchDir::open(temp.path()).expect("open scratch dir")
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_planted_symlink_at_the_name_is_replaced_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target");
        std::fs::write(&target, "ORIGINAL").unwrap();
        std::os::unix::fs::symlink(&target, temp.path().join("spur_job.sh")).unwrap();

        let path = write_scratch(
            &scratch(&temp),
            "spur_job.sh",
            b"new",
            ScratchOwner::Daemon,
            0o700,
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ORIGINAL");
        assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    // bash reads a script while it runs, so a rewrite must not change the file
    // an earlier run of the same job still has open.
    #[test]
    fn a_reader_of_the_old_file_keeps_its_bytes_after_a_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let dir = scratch(&temp);
        let path = write_scratch(
            &dir,
            "spur_job.sh",
            b"first version\n",
            ScratchOwner::Daemon,
            0o700,
        )
        .unwrap();
        let mut reader = File::open(&path).unwrap();

        write_scratch(
            &dir,
            "spur_job.sh",
            b"a longer second version\n",
            ScratchOwner::Daemon,
            0o700,
        )
        .unwrap();

        let mut held = String::new();
        reader.read_to_string(&mut held).unwrap();
        assert_eq!(held, "first version\n");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a longer second version\n"
        );
    }

    #[test]
    fn a_written_file_gets_the_requested_mode() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_scratch(
            &scratch(&temp),
            "spur_job.sh",
            b"#!/bin/bash\n",
            ScratchOwner::Daemon,
            0o700,
        )
        .unwrap();
        assert_eq!(mode_of(&path), 0o700);
    }

    #[test]
    fn a_directory_at_the_name_fails_without_an_errno_and_leaves_no_temp_file() {
        let temp = tempfile::tempdir().unwrap();
        let occupied = temp.path().join("spur_job.sh");
        std::fs::create_dir(&occupied).unwrap();
        std::fs::write(occupied.join("keep"), "x").unwrap();

        let error = write_scratch(
            &scratch(&temp),
            "spur_job.sh",
            b"new",
            ScratchOwner::Daemon,
            0o700,
        )
        .expect_err("a directory cannot be replaced by a file");

        assert_eq!(
            error.raw_os_error(),
            None,
            "an entry left in the folder must not read as the node's own I/O failure"
        );
        let entries: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("spur_job.sh")]);
    }

    #[test]
    fn output_replaces_a_planted_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target");
        std::fs::write(&target, "ORIGINAL").unwrap();
        std::os::unix::fs::symlink(&target, temp.path().join("step0.out")).unwrap();

        let mut out = open_scratch_output(&scratch(&temp), "step0.out", ScratchOwner::Daemon)
            .expect("open step output");
        out.write_all(b"step output").unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ORIGINAL");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("step0.out")).unwrap(),
            "step output"
        );
    }

    #[test]
    fn output_never_writes_through_a_hardlink() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("not-the-steps");
        std::fs::write(&original, "ORIGINAL").unwrap();
        std::fs::hard_link(&original, temp.path().join("step0.out")).unwrap();

        let mut out = open_scratch_output(&scratch(&temp), "step0.out", ScratchOwner::Daemon)
            .expect("open step output");
        out.write_all(b"step output").unwrap();

        assert_eq!(std::fs::read_to_string(&original).unwrap(), "ORIGINAL");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("step0.out")).unwrap(),
            "step output"
        );
    }

    #[test]
    fn output_is_private() {
        let temp = tempfile::tempdir().unwrap();
        open_scratch_output(&scratch(&temp), "step0.out", ScratchOwner::Daemon).unwrap();
        assert_eq!(mode_of(&temp.path().join("step0.out")), 0o600);
    }

    #[test]
    fn a_symlinked_subdirectory_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join("job7")).unwrap();

        let error = scratch(&temp)
            .subdir(OsStr::new("job7"), 0o750)
            .err()
            .expect("a symlinked folder must not be opened");

        assert!(
            matches!(
                error.raw_os_error(),
                Some(libc::ELOOP) | Some(libc::ENOTDIR)
            ),
            "{error}"
        );
    }

    #[test]
    fn a_subdirectory_is_created_once_and_reopened() {
        let temp = tempfile::tempdir().unwrap();
        let dir = scratch(&temp);
        let first = dir.subdir(OsStr::new("job7"), 0o750).unwrap();
        let again = dir.subdir(OsStr::new("job7"), 0o750).unwrap();
        assert_eq!(first.path(), again.path());
        assert!(temp.path().join("job7").is_dir());
    }

    #[test]
    fn a_path_beneath_a_root_is_created_component_by_component() {
        let root = tempfile::tempdir().unwrap();
        let staged =
            open_dir_beneath(root.path(), Path::new("var/spool/spur/job1"), 0o755).unwrap();
        assert_eq!(staged.path(), root.path().join("var/spool/spur/job1"));
        assert!(staged.path().is_dir());
    }

    // A container image is user-supplied, and an absolute link inside it would
    // resolve against the host when root walks the unpacked tree.
    #[test]
    fn an_absolute_symlink_inside_the_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(host.path(), root.path().join("var")).unwrap();

        let error = open_dir_beneath(root.path(), Path::new("var/spool/spur/job1"), 0o755)
            .err()
            .expect("the walk must stay inside the root");

        assert!(
            matches!(
                error.raw_os_error(),
                Some(libc::ELOOP) | Some(libc::ENOTDIR)
            ),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(host.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_parent_component_beneath_a_root_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let error = open_dir_beneath(root.path(), Path::new("../escape"), 0o755)
            .err()
            .expect("`..` must not be walked");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn reading_a_scratch_file_does_not_follow_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, "SECRET").unwrap();
        std::os::unix::fs::symlink(&secret, temp.path().join("cmd_0_1.sh")).unwrap();

        let error = read_scratch(&scratch(&temp), "cmd_0_1.sh")
            .expect_err("a planted link must not be read through");
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP), "{error}");
    }

    #[test]
    fn unlinking_removes_only_the_name() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target");
        std::fs::write(&target, "ORIGINAL").unwrap();
        std::os::unix::fs::symlink(&target, temp.path().join("cmd_0_1.sh")).unwrap();
        let dir = scratch(&temp);

        unlink_scratch(&dir, "cmd_0_1.sh").unwrap();
        unlink_scratch(&dir, "cmd_0_1.sh").expect("an absent name is already clean");

        assert!(!temp.path().join("cmd_0_1.sh").exists());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ORIGINAL");
    }

    #[test]
    fn a_root_daemon_claims_only_its_own_or_the_jobs_folder() {
        assert!(may_claim_spool_dir(true, 0, 1000));
        assert!(
            may_claim_spool_dir(true, 1000, 1000),
            "an older agent handed the folder to the job"
        );
        assert!(!may_claim_spool_dir(true, 1001, 1000));
        assert!(!may_claim_spool_dir(true, 1000, 0));
    }

    #[test]
    fn an_unprivileged_daemon_claims_any_folder_it_can_open() {
        assert!(may_claim_spool_dir(false, 1234, 1000));
    }

    #[tokio::test]
    async fn a_spool_file_is_read_without_following_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, "SECRET").unwrap();
        let planted = temp.path().join("step0.out");
        std::os::unix::fs::symlink(&secret, &planted).unwrap();
        let regular = temp.path().join("step1.out");
        std::fs::write(&regular, "output").unwrap();

        let error = open_spool_file(&planted)
            .await
            .expect_err("a planted link must not be read through");
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP), "{error}");
        open_spool_file(&regular)
            .await
            .expect("a regular spool file opens");
    }
}
