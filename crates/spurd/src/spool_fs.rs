// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use nix::fcntl::{openat, openat2, AtFlags, OFlag, OpenHow, ResolveFlag};
use nix::sys::stat::{fstatat, Mode};
use nix::unistd::{unlinkat, UnlinkatFlags};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, Instant};

// Bounds descriptors held by one walk; a deeper user tree is kept, not deleted.
pub(crate) const MAX_SPOOL_DEPTH: usize = 256;
const READ_CHUNK: usize = 1024;
const MAX_RESCANS: u8 = 2;
const COMPLETION_BUDGET: Duration = Duration::from_secs(60);

/// Opens a spool root, following symlinks in its path; nothing beneath it is followed.
pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

#[derive(Debug)]
struct Kept(&'static str);

impl std::fmt::Display for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Kept {}

const TOO_DEEP: &str = "deeper than the traversal limit";

pub(crate) fn budget_exhausted() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        Kept("cleanup time budget exhausted"),
    )
}

/// Why a job spool was kept rather than deleted, for the rare cases an admin must clean by hand.
pub(crate) fn kept_reason(error: &io::Error) -> Option<&'static str> {
    match error.raw_os_error() {
        Some(libc::EXDEV) => Some("contains a mount point, not crossed"),
        _ => error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<Kept>())
            .map(|kept| kept.0),
    }
}

pub(crate) fn open_child(parent: &File, name: &OsStr, flags: OFlag) -> io::Result<File> {
    if name.is_empty() || name == OsStr::new("..") || name.as_bytes().contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a direct spool entry",
        ));
    }
    let result = openat2(
        parent,
        name,
        OpenHow::new()
            .flags(flags | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_XDEV,
            ),
    );
    match result {
        Ok(entry) => Ok(entry.into()),
        Err(error) if unsupported_openat2(error) => {
            static FALLBACK: Once = Once::new();
            FALLBACK.call_once(
                || tracing::debug!(%error, "openat2 unavailable; using openat with mount checks"),
            );
            let entry: File = openat(
                parent,
                name,
                flags | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )?
            .into();
            // Device IDs do not distinguish bind mounts of the same filesystem.
            if mount_id(parent)? != mount_id(&entry)? {
                return Err(io::Error::from_raw_os_error(libc::EXDEV));
            }
            Ok(entry)
        }
        Err(error) => Err(error.into()),
    }
}

fn unsupported_openat2(error: nix::errno::Errno) -> bool {
    matches!(
        error,
        nix::errno::Errno::ENOSYS | nix::errno::Errno::EPERM | nix::errno::Errno::EINVAL
    )
}

fn mount_id(file: &File) -> io::Result<u64> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| io::Error::other("cannot establish spool entry mount identity"))
}

pub(crate) fn directory_names(directory: &File) -> io::Result<Vec<OsString>> {
    read_names(directory, usize::MAX)
}

fn read_names(directory: &File, limit: usize) -> io::Result<Vec<OsString>> {
    // A dup shares the directory offset; each read needs its own cursor.
    let cursor = open_child(
        directory,
        OsStr::new("."),
        OFlag::O_RDONLY | OFlag::O_DIRECTORY,
    )?;
    let mut directory = nix::dir::Dir::from_fd(cursor.into())?;
    directory
        .iter()
        .filter_map(|entry| match entry {
            Ok(entry) if matches!(entry.file_name().to_bytes(), b"." | b"..") => None,
            Ok(entry) => Some(Ok(
                OsStr::from_bytes(entry.file_name().to_bytes()).to_owned()
            )),
            Err(error) => Some(Err(error.into())),
        })
        .take(limit)
        .collect()
}

pub(crate) fn job_spool_name(job_id: u32) -> OsString {
    OsString::from(format!("job{job_id}"))
}

fn verify_root(root: &File) -> io::Result<()> {
    let metadata = root.metadata()?;
    if metadata.uid() != nix::unistd::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "spool root must be daemon-owned and not writable by group or others",
        ));
    }
    Ok(())
}

pub(crate) fn canonical_job_id(name: &OsStr) -> Option<u32> {
    let name = name.to_str()?;
    let id = name.strip_prefix("job")?.parse::<u32>().ok()?;
    (id > 0 && name == format!("job{id}")).then_some(id)
}

pub(crate) struct SpoolRoot {
    pub(crate) path: PathBuf,
    real_path: PathBuf,
    directory: File,
}

#[derive(Clone)]
pub(crate) struct JobSpool {
    pub(crate) job_id: u32,
    metadata: Metadata,
}

impl JobSpool {
    pub(crate) fn device(&self) -> u64 {
        self.metadata.dev()
    }
}

impl SpoolRoot {
    pub(crate) fn open(path: PathBuf) -> io::Result<Self> {
        let root = Self::open_for_owned_job(path)?;
        verify_root(&root.directory)?;
        Ok(root)
    }

    fn open_for_owned_job(path: PathBuf) -> io::Result<Self> {
        let directory = open_directory(&path)?;
        let real_path = std::fs::read_link(format!("/proc/self/fd/{}", directory.as_raw_fd()))?;
        Ok(Self {
            path,
            real_path,
            directory,
        })
    }

    /// The job's path with root symlinks resolved, matching how /proc reports holders.
    pub(crate) fn job_path(&self, job_id: u32) -> PathBuf {
        self.real_path.join(job_spool_name(job_id))
    }

    pub(crate) fn identity(&self) -> io::Result<(u64, u64)> {
        Ok(inode_identity(&self.directory.metadata()?))
    }

    pub(crate) fn candidates(&self) -> io::Result<Vec<JobSpool>> {
        let mut candidates = Vec::new();
        for name in directory_names(&self.directory)? {
            let Some(job_id) = canonical_job_id(&name) else {
                continue;
            };
            let metadata = match fstatat(
                &self.directory,
                name.as_os_str(),
                AtFlags::AT_SYMLINK_NOFOLLOW,
            ) {
                Ok(metadata) => metadata,
                Err(nix::errno::Errno::ENOENT) => continue,
                Err(error) => return Err(error.into()),
            };
            match metadata.st_mode & libc::S_IFMT {
                libc::S_IFDIR => {}
                libc::S_IFLNK => {
                    tracing::warn!(job_id, root = %self.path.display(),
                        "kept job spool: entry is a symlink, not followed");
                    continue;
                }
                _ => {
                    tracing::info!(job_id, root = %self.path.display(),
                        "skipped job spool name that is not a directory");
                    continue;
                }
            }
            match open_child(&self.directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY) {
                Ok(directory) => candidates.push(JobSpool {
                    job_id,
                    metadata: directory.metadata()?,
                }),
                Err(error) => tracing::warn!(job_id, root = %self.path.display(), %error,
                    "skipped unsafe or unreadable job spool"),
            }
        }
        Ok(candidates)
    }

    pub(crate) fn delete(&self, spool: &JobSpool, deadline: Option<Instant>) -> io::Result<()> {
        verify_root(&self.directory)?;
        if Self::open(self.path.clone())?.identity()? != self.identity()? {
            return Err(io::Error::other("spool root changed since discovery"));
        }
        self.delete_job(spool, deadline)
    }

    /// Whether any entry in the job's spool is one of `held`, the inodes live processes keep open.
    pub(crate) fn holds_any(
        &self,
        spool: &JobSpool,
        held: &HashSet<(u64, u64)>,
        deadline: Option<Instant>,
    ) -> io::Result<bool> {
        if held.is_empty() {
            return Ok(false);
        }
        let name = job_spool_name(spool.job_id);
        let directory =
            match open_child(&self.directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
        if !same_identity(&directory.metadata()?, &spool.metadata) {
            return Err(io::Error::other("job spool changed since discovery"));
        }
        if held.contains(&inode_identity(&spool.metadata)) {
            return Ok(true);
        }
        let mut stack = vec![ReadFrame::new(directory)?];
        while !stack.is_empty() {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(budget_exhausted());
            }
            let depth = stack.len();
            let Some(frame) = stack.last_mut() else {
                break;
            };
            let Some(entry) = frame.entries.next() else {
                stack.pop();
                continue;
            };
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            let metadata = match open_child(&frame.directory, name, OFlag::O_PATH) {
                Ok(entry) => entry.metadata()?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if held.contains(&inode_identity(&metadata)) {
                return Ok(true);
            }
            if !metadata.is_dir() {
                continue;
            }
            if depth >= MAX_SPOOL_DEPTH {
                return Err(io::Error::other(Kept(TOO_DEEP)));
            }
            let child = open_child(&frame.directory, name, OFlag::O_RDONLY | OFlag::O_DIRECTORY)?;
            if !same_identity(&child.metadata()?, &metadata) {
                return Err(io::Error::other("spool directory changed during inventory"));
            }
            stack.push(ReadFrame::new(child)?);
        }
        Ok(false)
    }

    fn delete_job(&self, spool: &JobSpool, deadline: Option<Instant>) -> io::Result<()> {
        let name = job_spool_name(spool.job_id);
        let directory =
            match open_child(&self.directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
        require_identity(&self.directory, &name, &spool.metadata)?;
        if !same_identity(&directory.metadata()?, &spool.metadata) {
            return Err(io::Error::other("job spool changed since discovery"));
        }
        remove_contents(&directory, deadline)?;
        require_identity(&self.directory, &name, &spool.metadata)?;
        absent_is_success(
            unlinkat(&self.directory, name.as_os_str(), UnlinkatFlags::RemoveDir)
                .map_err(Into::into),
        )
    }
}

struct ReadFrame {
    directory: File,
    entries: nix::dir::OwningIter,
}

impl ReadFrame {
    fn new(directory: File) -> io::Result<Self> {
        let cursor = open_child(
            &directory,
            OsStr::new("."),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY,
        )?;
        let entries = nix::dir::Dir::from_fd(cursor.into())?.into_iter();
        Ok(Self { directory, entries })
    }
}

pub(crate) fn inode_identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn same_identity(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn require_identity(parent: &File, name: &OsStr, expected: &Metadata) -> io::Result<()> {
    let current = fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
    if current.st_dev != expected.dev() || current.st_ino != expected.ino() {
        return Err(io::Error::other("spool entry changed during cleanup"));
    }
    Ok(())
}

fn remove_contents(directory: &File, deadline: Option<Instant>) -> io::Result<()> {
    let mut stack = vec![TraversalFrame::new(directory.try_clone()?, None)];
    while !stack.is_empty() {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(budget_exhausted());
        }
        let depth = stack.len();
        let Some(frame) = stack.last_mut() else {
            break;
        };
        let Some(name) = frame.next_name()? else {
            let completed = stack
                .pop()
                .ok_or_else(|| io::Error::other("missing spool traversal frame"))?;
            if let Some((name, metadata)) = completed.entry {
                let parent = stack
                    .last()
                    .ok_or_else(|| io::Error::other("missing spool parent"))?;
                require_identity(&parent.directory, &name, &metadata)?;
                absent_is_success(
                    unlinkat(
                        &parent.directory,
                        name.as_os_str(),
                        UnlinkatFlags::RemoveDir,
                    )
                    .map_err(Into::into),
                )?;
            }
            continue;
        };
        let entry = match open_child(&frame.directory, &name, OFlag::O_PATH) {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            if depth >= MAX_SPOOL_DEPTH {
                return Err(io::Error::other(Kept(TOO_DEEP)));
            }
            let child = open_child(
                &frame.directory,
                &name,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            )?;
            if !same_identity(&child.metadata()?, &metadata) {
                return Err(io::Error::other("spool directory changed during cleanup"));
            }
            stack.push(TraversalFrame::new(child, Some((name, metadata))));
            continue;
        }
        require_identity(&frame.directory, &name, &metadata)?;
        absent_is_success(
            unlinkat(
                &frame.directory,
                name.as_os_str(),
                UnlinkatFlags::NoRemoveDir,
            )
            .map_err(Into::into),
        )?;
    }
    Ok(())
}

struct TraversalFrame {
    directory: File,
    entry: Option<(OsString, Metadata)>,
    pending: std::vec::IntoIter<OsString>,
    previous: HashSet<OsString>,
    reached_end: bool,
    rescans: u8,
}

impl TraversalFrame {
    fn new(directory: File, entry: Option<(OsString, Metadata)>) -> Self {
        Self {
            directory,
            entry,
            pending: Vec::new().into_iter(),
            previous: HashSet::new(),
            reached_end: false,
            rescans: 0,
        }
    }

    // Each refill reads from the start: every name already returned has been unlinked.
    fn next_name(&mut self) -> io::Result<Option<OsString>> {
        if let Some(name) = self.pending.next() {
            return Ok(Some(name));
        }
        let names = read_names(&self.directory, READ_CHUNK)?;
        if names.is_empty() {
            return Ok(None);
        }
        // Every name returned before was unlinked, so seeing one again means no progress.
        if names.iter().any(|name| self.previous.contains(name)) {
            return Err(io::Error::other(Kept("kept changing during cleanup")));
        }
        self.previous = names.iter().cloned().collect();
        if self.reached_end {
            self.rescans += 1;
            if self.rescans > MAX_RESCANS {
                return Err(io::Error::other(Kept("kept changing during cleanup")));
            }
        }
        self.reached_end = names.len() < READ_CHUNK;
        self.pending = names.into_iter();
        Ok(self.pending.next())
    }
}

fn absent_is_success(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

pub(crate) fn cleanup_job_spool_under(root: &Path, job_id: u32) -> io::Result<()> {
    if job_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "job id must be positive",
        ));
    }
    // Completion owns this job's lifetime, so it skips startup's root ownership check.
    let root = match SpoolRoot::open_for_owned_job(root.to_owned()) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let name = job_spool_name(job_id);
    let directory = match open_child(&root.directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            let metadata = fstatat(
                &root.directory,
                name.as_os_str(),
                AtFlags::AT_SYMLINK_NOFOLLOW,
            );
            if metadata.is_ok_and(|metadata| metadata.st_mode & libc::S_IFMT == libc::S_IFLNK) {
                return Err(io::Error::other(Kept("entry is a symlink, not followed")));
            }
            return Err(error);
        }
    };
    root.delete_job(
        &JobSpool {
            job_id,
            metadata: directory.metadata()?,
        },
        Some(Instant::now() + COMPLETION_BUDGET),
    )
}

#[cfg(test)]
#[path = "spool_fs_tests.rs"]
mod tests;
