// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use nix::errno::Errno;
use nix::fcntl::{openat, openat2, renameat2, AtFlags, OFlag, OpenHow, RenameFlags, ResolveFlag};
use nix::mount::{umount2, MntFlags};
use nix::sys::stat::{fstatat, mkdirat, Mode};
use nix::unistd::{unlinkat, UnlinkatFlags};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};

// A rootfs moved here frees its job's path at once; `:` never occurs in a container name.
pub(crate) const TRASH_NAME: &str = "spur:trash";
// Bounds descriptors held by one walk; a deeper user tree is kept, not deleted.
pub(crate) const MAX_SPOOL_DEPTH: usize = 256;
const READ_CHUNK: usize = 1024;
const MAX_RESCANS: u8 = 2;
const COMPLETION_BUDGET: Duration = Duration::from_secs(60);
const MAX_UNMOUNT_ROUNDS: usize = 4;

/// Opens a root (spool, cgroup or session), following symlinks in its path; nothing beneath it is followed.
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

const CHANGED: &str = "changed since discovery";
const UNMOUNT_FAILED: &str = "a mount beneath it could not be unmounted";
const TOO_DEEP: &str = "deeper than the traversal limit";

/// When a walk must stop: a time budget, an agent shutdown, or neither.
#[derive(Clone, Default)]
pub(crate) struct Bound {
    deadline: Option<Instant>,
    stop: Option<Arc<AtomicBool>>,
}

impl Bound {
    pub(crate) fn until(deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
            stop: None,
        }
    }

    pub(crate) fn until_stopped(stop: Arc<AtomicBool>) -> Self {
        Self {
            deadline: None,
            stop: Some(stop),
        }
    }

    fn check(&self) -> io::Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(budget_exhausted());
        }
        if self
            .stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Relaxed))
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                Kept("stopped for agent shutdown"),
            ));
        }
        Ok(())
    }
}

pub(crate) fn budget_exhausted() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        Kept("cleanup time budget exhausted"),
    )
}

/// Why an entry was kept rather than deleted, for the rare cases an admin must clean by hand.
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

/// One line of a mountinfo file.
pub(crate) struct Mount {
    pub(crate) id: u64,
    pub(crate) device: String,
    pub(crate) root: PathBuf,
    pub(crate) point: PathBuf,
    pub(crate) kind: String,
}

/// Mounts listed in a mountinfo file; a missing file lists none.
pub(crate) fn read_mounts(path: &Path) -> io::Result<Vec<Mount>> {
    let info = match std::fs::read_to_string(path) {
        Ok(info) => info,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(info.lines().filter_map(parse_mount_line).collect())
}

fn parse_mount_line(line: &str) -> Option<Mount> {
    let (left, right) = line.split_once(" - ")?;
    let fields: Vec<_> = left.split(' ').collect();
    Some(Mount {
        id: fields.first()?.parse().ok()?,
        device: (*fields.get(2)?).to_owned(),
        root: PathBuf::from(unescape_mount_path(fields.get(3)?)),
        point: PathBuf::from(unescape_mount_path(fields.get(4)?)),
        kind: right.split(' ').next()?.to_owned(),
    })
}

fn unescape_mount_path(point: &str) -> String {
    point
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
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

/// Job id of an unnamed container rootfs: `job_<N>` (a job) or `step_<N>_<M>` (a step).
pub(crate) fn canonical_rootfs_job_id(name: &OsStr) -> Option<u32> {
    let name = name.to_str()?;
    let canonical = |s: &str| s.parse::<u32>().ok().filter(|n| s == n.to_string());
    if let Some(job) = name.strip_prefix("job_") {
        return canonical(job).filter(|id| *id > 0);
    }
    let (job, step) = name.strip_prefix("step_")?.split_once('_')?;
    canonical(step)?;
    canonical(job).filter(|id| *id > 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SpoolKind {
    /// `job<N>` under a spool root: the job script and step output.
    Spool,
    /// `job_<N>` or `step_<N>_<M>` under the container root: an unnamed container rootfs.
    Rootfs,
}

impl SpoolKind {
    fn job_id(self, name: &OsStr) -> Option<u32> {
        match self {
            Self::Spool => canonical_job_id(name),
            Self::Rootfs => canonical_rootfs_job_id(name),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Spool => "job spool",
            Self::Rootfs => "container rootfs",
        }
    }
}

pub(crate) struct SpoolRoot {
    pub(crate) path: PathBuf,
    real_path: PathBuf,
    directory: File,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Unmounted {
    pub(crate) count: usize,
    /// Whether a mount sat on the entry itself, so the directory underneath is a new identity.
    pub(crate) base: bool,
}

#[derive(Clone)]
pub(crate) struct JobSpool {
    pub(crate) job_id: u32,
    pub(crate) kind: SpoolKind,
    name: OsString,
    metadata: Metadata,
    roots: Vec<(u64, u64)>,
}

impl JobSpool {
    /// Inodes a process would hold as its root or cwd while using this entry.
    pub(crate) fn identities(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        std::iter::once(inode_identity(&self.metadata)).chain(self.roots.iter().copied())
    }
}

// An overlay rootfs is entered through `merged`, a separate filesystem from the base.
fn rootfs_roots(kind: SpoolKind, base: &File) -> io::Result<Vec<(u64, u64)>> {
    if kind != SpoolKind::Rootfs {
        return Ok(Vec::new());
    }
    match fstatat(base, "merged", AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) if stat.st_mode & libc::S_IFMT == libc::S_IFDIR => {
            Ok(vec![(stat.st_dev, stat.st_ino)])
        }
        Ok(_) | Err(nix::errno::Errno::ENOENT) => Ok(Vec::new()),
        Err(error) => Err(error.into()),
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

    /// The entry's path with root symlinks resolved, matching how /proc and mountinfo report it.
    pub(crate) fn entry_path(&self, spool: &JobSpool) -> PathBuf {
        self.real_path.join(&spool.name)
    }

    pub(crate) fn identity(&self) -> io::Result<(u64, u64)> {
        Ok(inode_identity(&self.directory.metadata()?))
    }

    /// Unmounts every mount at or beneath the entry in the agent's namespace, deepest first;
    /// any that remain keep it.
    pub(crate) fn unmount_beneath(
        &self,
        spool: &JobSpool,
        mountinfo: &Path,
    ) -> io::Result<Unmounted> {
        let base = self.entry_path(spool);
        let beneath = |mounts: Vec<Mount>| -> Vec<PathBuf> {
            let mut points: Vec<_> = mounts
                .into_iter()
                .map(|mount| mount.point)
                .filter(|point| point.starts_with(&base))
                .collect();
            points.sort_by_key(|point| std::cmp::Reverse(point.as_os_str().len()));
            points
        };
        let mut detached = Unmounted::default();
        for _ in 0..MAX_UNMOUNT_ROUNDS {
            let points = beneath(read_mounts(mountinfo)?);
            if points.is_empty() {
                return Ok(detached);
            }
            for point in points {
                match umount2(&point, MntFlags::UMOUNT_NOFOLLOW) {
                    Ok(()) => {
                        detached.count += 1;
                        detached.base |= point == base;
                    }
                    Err(Errno::EINVAL | Errno::ENOENT) => {}
                    Err(error) => {
                        tracing::warn!(job_id = spool.job_id, mount = %point.display(), %error,
                            "could not unmount a mount beneath an orphan entry");
                        return Err(io::Error::other(Kept(UNMOUNT_FAILED)));
                    }
                }
            }
        }
        let remaining = beneath(read_mounts(mountinfo)?);
        if remaining.is_empty() {
            return Ok(detached);
        }
        tracing::warn!(job_id = spool.job_id, mounts = ?remaining,
            "mounts beneath an orphan entry would not unmount");
        Err(io::Error::other(Kept(UNMOUNT_FAILED)))
    }

    pub(crate) fn candidates(&self, kind: SpoolKind) -> io::Result<Vec<JobSpool>> {
        let mut candidates = Vec::new();
        for name in directory_names(&self.directory)? {
            let Some(job_id) = kind.job_id(&name) else {
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
                        "kept {}: entry is a symlink, not followed", kind.label());
                    continue;
                }
                _ => {
                    tracing::info!(job_id, root = %self.path.display(),
                        "skipped {} name that is not a directory", kind.label());
                    continue;
                }
            }
            let opened = self
                .open_entry(kind, &name)
                .and_then(|directory| Ok((rootfs_roots(kind, &directory)?, directory.metadata()?)));
            match opened {
                Ok((roots, metadata)) => candidates.push(JobSpool {
                    job_id,
                    kind,
                    roots,
                    metadata,
                    name,
                }),
                Err(error) => tracing::warn!(job_id, root = %self.path.display(), %error,
                    "skipped unsafe or unreadable {}", kind.label()),
            }
        }
        Ok(candidates)
    }

    // A rootfs base may itself be a mount point; it is unmounted before anything is deleted.
    fn open_entry(&self, kind: SpoolKind, name: &OsStr) -> io::Result<File> {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY;
        match open_child(&self.directory, name, flags) {
            Err(error)
                if kind == SpoolKind::Rootfs && error.raw_os_error() == Some(libc::EXDEV) =>
            {
                Ok(openat(
                    &self.directory,
                    name,
                    flags | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                    Mode::empty(),
                )?
                .into())
            }
            result => result,
        }
    }

    /// The entry after unmounting; only a mount removed from the entry itself may change its identity.
    pub(crate) fn reopen(&self, spool: &JobSpool, unmounted: Unmounted) -> io::Result<JobSpool> {
        let directory = open_child(
            &self.directory,
            &spool.name,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY,
        )?;
        let metadata = directory.metadata()?;
        if !unmounted.base && !same_identity(&metadata, &spool.metadata) {
            return Err(io::Error::other(Kept(CHANGED)));
        }
        Ok(JobSpool {
            roots: rootfs_roots(spool.kind, &directory)?,
            metadata,
            ..spool.clone()
        })
    }

    /// The private directory that rootfs entries are moved into before deletion.
    pub(crate) fn trash(&self) -> io::Result<SpoolRoot> {
        match mkdirat(&self.directory, TRASH_NAME, Mode::S_IRWXU) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(error) => return Err(error.into()),
        }
        self.open_trash()
    }

    fn open_trash(&self) -> io::Result<SpoolRoot> {
        let name = OsStr::new(TRASH_NAME);
        let directory = open_child(&self.directory, name, OFlag::O_RDONLY | OFlag::O_DIRECTORY)?;
        verify_root(&directory)?;
        Ok(SpoolRoot {
            path: self.path.join(name),
            real_path: self.real_path.join(name),
            directory,
        })
    }

    /// Moves the entry into `trash` under a fresh name; the moved entry keeps its identity.
    pub(crate) fn move_to(&self, spool: &JobSpool, trash: &SpoolRoot) -> io::Result<JobSpool> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        require_identity(&self.directory, &spool.name, &spool.metadata)?;
        let mut name = spool.name.clone();
        name.push(format!(
            ".{}.{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        renameat2(
            &self.directory,
            spool.name.as_os_str(),
            &trash.directory,
            name.as_os_str(),
            RenameFlags::RENAME_NOREPLACE,
        )?;
        require_identity(&trash.directory, &name, &spool.metadata)?;
        Ok(JobSpool {
            name,
            ..spool.clone()
        })
    }

    /// Entries an interrupted run left in the trash; none if there is no trash yet.
    pub(crate) fn trash_leftovers(&self) -> io::Result<Option<(SpoolRoot, Vec<JobSpool>)>> {
        let trash = match self.open_trash() {
            Ok(trash) => trash,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut leftovers = Vec::new();
        for name in directory_names(&trash.directory)? {
            let job_id = name
                .to_str()
                .and_then(|name| name.split_once('.'))
                .and_then(|(original, _)| canonical_rootfs_job_id(OsStr::new(original)))
                .unwrap_or(0);
            let directory = match open_child(
                &trash.directory,
                &name,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            ) {
                Ok(directory) => directory,
                Err(error) => {
                    tracing::warn!(root = %trash.path.display(), entry = ?name, %error,
                            "skipped unexpected container trash entry");
                    continue;
                }
            };
            leftovers.push(JobSpool {
                job_id,
                kind: SpoolKind::Rootfs,
                metadata: directory.metadata()?,
                roots: Vec::new(),
                name,
            });
        }
        Ok(Some((trash, leftovers)))
    }

    pub(crate) fn device(&self) -> io::Result<u64> {
        Ok(self.directory.metadata()?.dev())
    }

    pub(crate) fn delete(&self, spool: &JobSpool, bound: &Bound) -> io::Result<()> {
        verify_root(&self.directory)?;
        if Self::open(self.path.clone())?.identity()? != self.identity()? {
            return Err(io::Error::other(Kept(CHANGED)));
        }
        self.delete_job(spool, bound)
    }

    /// Whether any entry in the job's spool is one of `held`, the inodes live processes keep open.
    pub(crate) fn holds_any(
        &self,
        spool: &JobSpool,
        held: &HashSet<(u64, u64)>,
        bound: &Bound,
    ) -> io::Result<bool> {
        if held.is_empty() {
            return Ok(false);
        }
        let name = &spool.name;
        let directory =
            match open_child(&self.directory, name, OFlag::O_RDONLY | OFlag::O_DIRECTORY) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
        if !same_identity(&directory.metadata()?, &spool.metadata) {
            return Err(io::Error::other(Kept(CHANGED)));
        }
        if held.contains(&inode_identity(&spool.metadata)) {
            return Ok(true);
        }
        let mut stack = vec![ReadFrame::new(directory)?];
        while !stack.is_empty() {
            bound.check()?;
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

    fn delete_job(&self, spool: &JobSpool, bound: &Bound) -> io::Result<()> {
        let name = spool.name.clone();
        let directory =
            match open_child(&self.directory, &name, OFlag::O_RDONLY | OFlag::O_DIRECTORY) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
        require_identity(&self.directory, &name, &spool.metadata)?;
        if !same_identity(&directory.metadata()?, &spool.metadata) {
            return Err(io::Error::other(Kept(CHANGED)));
        }
        remove_contents(&directory, bound)?;
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

fn remove_contents(directory: &File, bound: &Bound) -> io::Result<()> {
    let mut stack = vec![TraversalFrame::new(directory.try_clone()?, None)];
    while !stack.is_empty() {
        bound.check()?;
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

    fn next_name(&mut self) -> io::Result<Option<OsString>> {
        if let Some(name) = self.pending.next() {
            return Ok(Some(name));
        }
        let names = read_names(&self.directory, READ_CHUNK)?;
        if names.is_empty() {
            return Ok(None);
        }
        // Each refill reads from the start; every name returned before was unlinked, so a repeat
        // means no progress.
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
            kind: SpoolKind::Spool,
            metadata: directory.metadata()?,
            roots: Vec::new(),
            name,
        },
        &Bound::until(Instant::now() + COMPLETION_BUDGET),
    )
}

#[cfg(test)]
#[path = "spool_fs_tests.rs"]
mod tests;
